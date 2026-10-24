//! Undo and redo history for editor edits.
//!
//! # Responsibilities
//!
//! - Own the ordered undo entries, the apply cursor, and the redo tail.
//! - Merge continuous edits (a slider drag) into one entry by coalescing.
//! - Enforce the step cap and the memory budget from the preferences (D31),
//!   dropping the oldest entries first and saying which limit did it.
//! - Drop entries addressing component types or fields that a reload removed.
//!
//! # Design
//!
//! The history is a world resource, so it survives reloads of the editor
//! modules that write into it. An entry holds engine batches in both
//! directions; undo pops an entry and queues its inverse batch through the
//! same frame-boundary application every edit uses, so undo can never mutate
//! differently from the edit it reverses. A restore of a destroyed entity
//! assigns a fresh handle; the queue's caller remaps the remaining entries
//! with [`CommandHistory::remap_entity`].

// Standard library
use std::time::Duration;

// External crates
use pill_engine::{Entity, FieldValue};

// Current crate
use crate::editor_state::EditorCommand;

/// One undoable step.
#[derive(Clone, Debug)]
pub struct HistoryEntry {
    /// Human label shown in menus and logs ("set PhysicsState.position_x").
    pub label: String,
    /// What undoing and redoing this step does.
    pub entry: HistoryEntryKind,
    /// Component and field identifiers the entry addresses.
    pub touched: Vec<String>,
    /// Approximate retained size, counting asset pre-images where they exist.
    pub memory_bytes: usize,
    /// Continuous edits sharing a key merge while they arrive back to back.
    pub coalesce_key: Option<String>,
    /// Arrival time of the newest coalesced edit, in milliseconds.
    pub coalesced_at_millis: u64,
}

/// The two kinds of undoable step.
#[derive(Clone, Debug)]
pub enum HistoryEntryKind {
    /// An engine batch and its inverse, in undo order.
    Engine {
        /// The commands the edit applied.
        forward: Vec<EditorCommand>,
        /// The commands that reverse it, in undo order.
        inverse: Vec<EditorCommand>,
    },
    /// A selection change; undo restores `before`, redo applies `after`.
    Selection {
        /// The selection before the change.
        before: Option<Entity>,
        /// The selection after the change.
        after: Option<Entity>,
    },
}

impl HistoryEntry {
    /// An engine-batch entry.
    pub fn engine(
        label: impl Into<String>,
        forward: Vec<EditorCommand>,
        inverse: Vec<EditorCommand>,
    ) -> Self {
        let touched = forward
            .iter()
            .flat_map(EditorCommand::touched_identifiers)
            .collect();
        Self {
            label: label.into(),
            memory_bytes: estimate_bytes(&forward) + estimate_bytes(&inverse),
            entry: HistoryEntryKind::Engine { forward, inverse },
            touched,
            coalesce_key: None,
            coalesced_at_millis: 0,
        }
    }

    /// A selection-change entry.
    pub fn selection(before: Option<Entity>, after: Option<Entity>) -> Self {
        Self {
            label: "select".to_string(),
            entry: HistoryEntryKind::Selection { before, after },
            touched: Vec::new(),
            memory_bytes: 64,
            coalesce_key: None,
            coalesced_at_millis: 0,
        }
    }

    /// Mark this entry as part of a continuous edit sequence.
    pub fn coalescing(mut self, key: impl Into<String>, at_millis: u64) -> Self {
        self.coalesce_key = Some(key.into());
        self.coalesced_at_millis = at_millis;
        self
    }

    /// Replace every named handle with `assigned`.
    pub fn remap_entity(&mut self, previous: Entity, assigned: Entity) {
        match &mut self.entry {
            HistoryEntryKind::Engine { forward, inverse } => {
                for command in forward.iter_mut().chain(inverse.iter_mut()) {
                    command.remap_entity(previous, assigned);
                }
            }
            HistoryEntryKind::Selection { before, after } => {
                if *before == Some(previous) {
                    *before = Some(assigned);
                }
                if *after == Some(previous) {
                    *after = Some(assigned);
                }
            }
        }
    }
}

/// Approximate the retained size of a batch.
///
/// The estimate only gates the memory budget, so exactness is not required:
/// a fixed per-command cost plus the owned strings and array payloads gets
/// the budget's behavior (old entries drop under pressure) right.
pub fn estimate_bytes(commands: &[EditorCommand]) -> usize {
    const PER_COMMAND: usize = 96;
    let mut bytes = commands.len() * PER_COMMAND;
    for command in commands {
        let strings = match command {
            EditorCommand::SetField {
                component,
                field,
                value,
                ..
            }
            | EditorCommand::SetArrayElement {
                component,
                field,
                value,
                ..
            } => component.len() + field.len() + field_value_bytes(value),
            EditorCommand::SetFieldAsset {
                component,
                field,
                reference,
                ..
            } => component.len() + field.len() + reference.as_ref().map_or(0, String::len),
            EditorCommand::SetFieldEntity {
                component, field, ..
            } => component.len() + field.len(),
            EditorCommand::ArmEntityPick {
                component, field, ..
            } => component.len() + field.len(),
            EditorCommand::AddComponent { component, .. }
            | EditorCommand::RemoveComponent { component, .. }
            | EditorCommand::RestoreComponent { component, .. } => component.len(),
            EditorCommand::CreateEntity { components }
            | EditorCommand::RestoreEntity { components, .. } => components
                .iter()
                .map(|seed| {
                    seed.type_name.len()
                        + seed
                            .fields
                            .iter()
                            .map(|(name, value)| name.len() + field_value_bytes(value))
                            .sum::<usize>()
                })
                .sum(),
            EditorCommand::DestroyEntity { .. }
            | EditorCommand::SetSystemEnabled { .. }
            | EditorCommand::SetPaused { .. }
            | EditorCommand::SetGizmoTool { .. }
            | EditorCommand::SetGrid { .. }
            | EditorCommand::StepOnce => 0,
            EditorCommand::SetPreferences { values } => values
                .iter()
                .map(|(key, text)| key.len() + text.len())
                .sum(),
            EditorCommand::ImportFiles {
                sources,
                destination,
            } => sources.iter().map(String::len).sum::<usize>() + destination.len(),
            EditorCommand::RemoveFiles {
                paths,
                sources,
                destination,
            } => {
                paths.iter().map(String::len).sum::<usize>()
                    + sources.iter().map(String::len).sum::<usize>()
                    + destination.len()
            }
            // The stage 25 asset commands: the restore carries the deleted
            // file's whole pre-image, which is what the budget counts.
            EditorCommand::CreateStandaloneAsset {
                type_name,
                folder,
                name,
            } => type_name.len() + folder.len() + name.len(),
            EditorCommand::DeleteAssetFile { path } | EditorCommand::WriteAssetMeta { path } => {
                path.len()
            }
            EditorCommand::RestoreAssetFile { path, bytes, meta } => {
                path.len() + bytes.len() + meta.as_ref().map(Vec::len).unwrap_or(0)
            }
            EditorCommand::MoveAssetFile { from, to } => from.len() + to.len(),
            EditorCommand::SetAssetSettings { path, settings } => {
                path.len() + settings.to_string().len()
            }
            EditorCommand::SetResourceField {
                resource,
                field,
                value,
            } => resource.len() + field.len() + field_value_bytes(value),
            EditorCommand::SetSelection { entities, asset } => {
                entities.len() * 16 + asset.as_ref().map(String::len).unwrap_or(0)
            }
            EditorCommand::SelectResource { resource } => {
                resource.as_ref().map(String::len).unwrap_or(0)
            }
            EditorCommand::RestoreSelection {
                entities,
                asset,
                resource,
            } => {
                entities.len() * 16
                    + asset.as_ref().map(String::len).unwrap_or(0)
                    + resource.as_ref().map(String::len).unwrap_or(0)
            }
        };
        bytes += strings;
    }
    bytes
}

/// Byte size of one field value payload (strings and arrays dominate).
fn field_value_bytes(value: &FieldValue) -> usize {
    match value {
        FieldValue::Text(text) => text.len(),
        FieldValue::Array(elements) => elements.iter().map(field_value_bytes).sum(),
        _ => 8,
    }
}

/// Why the history dropped its oldest entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrimReason {
    /// The step cap was reached.
    Steps,
    /// The memory budget was reached.
    Memory,
}

/// What [`CommandHistory::push`] did to the history.
#[derive(Clone, Copy, Debug, Default)]
pub struct PushOutcome {
    /// Entries still in the history after the push.
    pub steps: usize,
    /// How many oldest entries this push dropped.
    pub trimmed: usize,
    /// The limit that caused the drop, when one did.
    pub trim_reason: Option<TrimReason>,
    /// True when this push merged into the previous entry.
    pub coalesced: bool,
}

/// The editor's undo stack.
#[derive(Debug)]
pub struct CommandHistory {
    /// Oldest first; `entries[cursor]` is the next redo.
    entries: Vec<HistoryEntry>,
    /// Number of entries currently applied.
    cursor: usize,
    /// Maximum number of entries.
    step_cap: usize,
    /// Maximum retained bytes.
    memory_budget_bytes: usize,
    /// Sum of the entries' `memory_bytes`.
    memory_used_bytes: usize,
    /// The component-registry stamp `drop_stale` last pruned against; zero
    /// before the first prune, so the first call always runs.
    registry_stamp: u64,
}

impl Default for CommandHistory {
    fn default() -> Self {
        Self::new(Self::DEFAULT_STEP_CAP, Self::DEFAULT_MEMORY_BUDGET_BYTES)
    }
}

impl CommandHistory {
    /// Steps kept when the preferences do not say otherwise.
    pub const DEFAULT_STEP_CAP: usize = 500;
    /// Bytes kept when the preferences do not say otherwise.
    pub const DEFAULT_MEMORY_BUDGET_BYTES: usize = 256 * 1024 * 1024;
    /// Continuous edits within this window merge into one step.
    pub const DEFAULT_COALESCE_WINDOW: Duration = Duration::from_millis(400);

    /// A history enforcing the given limits.
    pub fn new(step_cap: usize, memory_budget_bytes: usize) -> Self {
        Self {
            entries: Vec::new(),
            cursor: 0,
            step_cap,
            memory_budget_bytes,
            memory_used_bytes: 0,
            registry_stamp: 0,
        }
    }

    /// Re-claim the resource across a reload, keeping the live history.
    pub fn ensure(world: &mut pill_engine::World) {
        world.register_resource::<CommandHistory>();
        if world.get_resource::<CommandHistory>().is_none() {
            world.insert_resource(Self::default());
        }
    }

    /// Apply the preferences' limits (D31), trimming if they shrank.
    ///
    /// Returns the trim that resulted, for the caller to log.
    pub fn set_limits(&mut self, step_cap: usize, memory_budget_bytes: usize) -> PushOutcome {
        self.step_cap = step_cap.max(1);
        self.memory_budget_bytes = memory_budget_bytes;
        let (trimmed, trim_reason) = self.trim();
        PushOutcome {
            steps: self.entries.len(),
            trimmed,
            trim_reason,
            coalesced: false,
        }
    }

    /// Push one step, merging it into the previous one when they coalesce.
    ///
    /// Pushing after an undo drops the redo tail: the branch the user left is
    /// gone, which is what every editor does.
    pub fn push(
        &mut self,
        mut entry: HistoryEntry,
        now_millis: u64,
        coalesce_window: Duration,
    ) -> PushOutcome {
        let mut outcome = PushOutcome::default();
        // Step 1: decide whether this merges into the newest applied entry.
        // Only a push at the tip merges; anything after an undo starts fresh.
        let merge_index = if self.cursor > 0 && self.cursor == self.entries.len() {
            Some(self.cursor - 1)
        } else {
            None
        };
        if let (Some(index), Some(key)) = (merge_index, entry.coalesce_key.as_deref()) {
            let last = &mut self.entries[index];
            let within_window = now_millis.saturating_sub(last.coalesced_at_millis)
                <= coalesce_window.as_millis() as u64;
            if last.coalesce_key.as_deref() == Some(key) && within_window {
                if let (
                    HistoryEntryKind::Engine { forward, .. },
                    HistoryEntryKind::Engine {
                        forward: new_forward,
                        inverse: new_inverse,
                    },
                ) = (&mut last.entry, &mut entry.entry)
                {
                    // Keep the original inverse (it restores the drag's start)
                    // and adopt the newest forward values.
                    let merged_bytes = estimate_bytes(new_forward) + estimate_bytes(new_inverse);
                    self.memory_used_bytes = self
                        .memory_used_bytes
                        .saturating_sub(last.memory_bytes)
                        .saturating_add(merged_bytes);
                    last.memory_bytes = merged_bytes;
                    *forward = std::mem::take(new_forward);
                    for identifier in &entry.touched {
                        if !last.touched.contains(identifier) {
                            last.touched.push(identifier.clone());
                        }
                    }
                    last.coalesced_at_millis = now_millis;
                    outcome.coalesced = true;
                    outcome.steps = self.entries.len();
                    return outcome;
                }
            }
        }
        // Step 2: a fresh push discards the abandoned redo tail.
        if self.cursor < self.entries.len() {
            let dropped: Vec<HistoryEntry> = self.entries.split_off(self.cursor);
            for entry in &dropped {
                self.memory_used_bytes = self.memory_used_bytes.saturating_sub(entry.memory_bytes);
            }
        }
        // Step 3: record, then enforce the limits oldest-first.
        self.memory_used_bytes = self.memory_used_bytes.saturating_add(entry.memory_bytes);
        self.entries.push(entry);
        self.cursor = self.entries.len();
        let (trimmed, trim_reason) = self.trim();
        outcome.trimmed = trimmed;
        outcome.trim_reason = trim_reason;
        outcome.steps = self.entries.len();
        outcome
    }

    /// Drop oldest entries until both limits hold.
    fn trim(&mut self) -> (usize, Option<TrimReason>) {
        let mut trimmed = 0;
        let mut reason = None;
        while self.entries.len() > self.step_cap
            || self.memory_used_bytes > self.memory_budget_bytes
        {
            let entry = self.entries.remove(0);
            self.memory_used_bytes = self.memory_used_bytes.saturating_sub(entry.memory_bytes);
            self.cursor = self.cursor.saturating_sub(1);
            trimmed += 1;
            // Report the limit that would still be violated first: a step
            // cap stop is the common case and must not hide behind memory.
            if self.entries.len() >= self.step_cap {
                reason = Some(TrimReason::Steps);
            } else if reason.is_none() {
                reason = Some(TrimReason::Memory);
            }
        }
        (trimmed, reason)
    }

    /// Pop the newest applied step for undoing.
    pub fn pop_undo(&mut self) -> Option<HistoryEntry> {
        if self.cursor == 0 {
            return None;
        }
        self.cursor -= 1;
        Some(self.entries[self.cursor].clone())
    }

    /// Pop the next unapplied step for redoing.
    pub fn pop_redo(&mut self) -> Option<HistoryEntry> {
        if self.cursor >= self.entries.len() {
            return None;
        }
        let entry = self.entries[self.cursor].clone();
        self.cursor += 1;
        Some(entry)
    }

    /// Entries currently held (applied plus redoable).
    pub fn steps(&self) -> usize {
        self.entries.len()
    }

    /// Steps available to undo.
    pub fn undo_depth(&self) -> usize {
        self.cursor
    }

    /// Steps available to redo.
    pub fn redo_depth(&self) -> usize {
        self.entries.len() - self.cursor
    }

    /// Retained size in bytes.
    pub fn memory_bytes(&self) -> usize {
        self.memory_used_bytes
    }

    /// Follow an entity to the fresh handle a restore assigned it.
    pub fn remap_entity(&mut self, previous: Entity, assigned: Entity) {
        for entry in &mut self.entries {
            entry.remap_entity(previous, assigned);
        }
    }

    /// Drop entries whose identifiers a reload invalidated.
    ///
    /// `is_valid` receives `"component"` and `"component.field"` identifiers;
    /// an entry with any invalid identifier goes, because undoing it would
    /// target storage that no longer exists. `stamp` identifies the registry
    /// the check ran against: an unchanged stamp skips the walk, so a caller
    /// may ask on every frame for one hash and no allocation. Returns how
    /// many were dropped.
    pub fn drop_stale(&mut self, is_valid: impl Fn(&str) -> bool, stamp: u64) -> usize {
        if self.registry_stamp == stamp {
            return 0;
        }
        self.registry_stamp = stamp;
        let applied_before = self.cursor;
        let mut dropped = 0;
        let mut kept: Vec<HistoryEntry> = Vec::with_capacity(self.entries.len());
        let mut applied_kept = 0;
        for (index, entry) in std::mem::take(&mut self.entries).into_iter().enumerate() {
            if entry.touched.iter().any(|identifier| !is_valid(identifier)) {
                dropped += 1;
                self.memory_used_bytes = self.memory_used_bytes.saturating_sub(entry.memory_bytes);
                continue;
            }
            if index < applied_before {
                applied_kept += 1;
            }
            kept.push(entry);
        }
        self.entries = kept;
        self.cursor = applied_kept;
        dropped
    }
}

impl pill_engine::Resource for CommandHistory {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_state::EditorCommand;

    /// A field edit entry over a named component, for history-only tests.
    fn field_entry(label: &str, value: f32) -> HistoryEntry {
        let command = EditorCommand::SetField {
            entity: Entity::default(),
            component: "Demo".to_string(),
            field: "strength".to_string(),
            value: FieldValue::F32(value),
        };
        HistoryEntry::engine(label, vec![command.clone()], vec![command])
    }

    /// The window tests coalesce against, in milliseconds.
    const WINDOW: Duration = Duration::from_millis(400);

    #[test]
    fn the_memory_budget_counts_asset_pre_images() {
        // The undo of an asset delete carries the whole file as its
        // pre-image; that is the memory the budget must count (Stage 25).
        let pre_image = vec![0u8; 64 * 1024];
        let forward = EditorCommand::DeleteAssetFile {
            path: "textures/helmet.png".to_string(),
        };
        let inverse = EditorCommand::RestoreAssetFile {
            path: "textures/helmet.png".to_string(),
            bytes: pre_image.clone(),
            meta: Some(b"{\"guid\":\"abc\"}".to_vec()),
        };
        let entry = HistoryEntry::engine("delete asset", vec![forward], vec![inverse]);
        assert!(
            entry.memory_bytes >= pre_image.len(),
            "the pre-image must land in memory_bytes ({} < {})",
            entry.memory_bytes,
            pre_image.len()
        );

        let mut history = CommandHistory::new(10, 32 * 1024);
        history.push(entry.clone(), 0, WINDOW);
        assert_eq!(
            history.steps(),
            0,
            "an entry whose pre-image exceeds the budget is dropped"
        );
        // The same entry fits a larger budget: the drop above was the byte
        // count, not the recording.
        let mut roomy = CommandHistory::new(10, usize::MAX);
        roomy.push(entry, 0, WINDOW);
        assert_eq!(roomy.steps(), 1);
        assert!(roomy.memory_bytes() >= pre_image.len());
    }

    #[test]
    fn push_undo_redo_round_trip() {
        let mut history = CommandHistory::new(10, usize::MAX);
        history.push(field_entry("one", 1.0), 0, WINDOW);
        history.push(field_entry("two", 2.0), 1000, WINDOW);
        assert_eq!(history.steps(), 2);
        assert_eq!(history.undo_depth(), 2);

        let undone = history.pop_undo().expect("second step");
        assert_eq!(undone.label, "two");
        assert_eq!(history.undo_depth(), 1);
        assert_eq!(history.redo_depth(), 1);

        let redone = history.pop_redo().expect("redo");
        assert_eq!(redone.label, "two");
        assert_eq!(history.redo_depth(), 0);
        assert!(history.pop_redo().is_none());
    }

    #[test]
    fn push_after_undo_drops_the_redo_tail() {
        let mut history = CommandHistory::new(10, usize::MAX);
        history.push(field_entry("one", 1.0), 0, WINDOW);
        history.push(field_entry("two", 2.0), 1000, WINDOW);
        history.pop_undo();
        history.push(field_entry("three", 3.0), 2000, WINDOW);
        assert_eq!(history.steps(), 2);
        assert_eq!(history.redo_depth(), 0);
        assert_eq!(history.undo_depth(), 2);
    }

    #[test]
    fn continuous_edits_coalesce_into_one_step() {
        let mut history = CommandHistory::new(10, usize::MAX);
        // A slider drag: same key, edits close together in time.
        for step in 0..5 {
            let entry = field_entry("drag", step as f32).coalescing("slider", step * 50);
            history.push(entry, step * 50, WINDOW);
        }
        // One step; the inverse still restores the drag's starting value.
        assert_eq!(history.steps(), 1);
        let entry = history.pop_undo().expect("drag step");
        let HistoryEntryKind::Engine { forward, inverse } = entry.entry else {
            panic!("engine entry expected");
        };
        assert_eq!(
            forward,
            vec![EditorCommand::SetField {
                entity: Entity::default(),
                component: "Demo".to_string(),
                field: "strength".to_string(),
                value: FieldValue::F32(4.0),
            }]
        );
        assert_eq!(
            inverse,
            vec![EditorCommand::SetField {
                entity: Entity::default(),
                component: "Demo".to_string(),
                field: "strength".to_string(),
                value: FieldValue::F32(0.0),
            }]
        );

        // A gap longer than the window splits the drag into a new step.
        history.push(
            field_entry("drag", 9.0).coalescing("slider", 5000),
            5000,
            WINDOW,
        );
        assert_eq!(history.steps(), 1);
    }

    #[test]
    fn step_cap_trims_oldest_first() {
        let mut history = CommandHistory::new(3, usize::MAX);
        for step in 0..5 {
            history.push(field_entry("step", step as f32), step as u64 * 1000, WINDOW);
        }
        assert_eq!(history.steps(), 3);
        let outcome = history.push(field_entry("more", 9.0), 6000, WINDOW);
        assert_eq!(outcome.trimmed, 1);
        assert_eq!(outcome.trim_reason, Some(TrimReason::Steps));
    }

    #[test]
    fn memory_budget_trims_with_its_own_reason() {
        let mut history = CommandHistory::new(100, 2048);
        for step in 0..8 {
            let command = EditorCommand::SetField {
                entity: Entity::default(),
                component: "Demo".to_string(),
                field: "name".to_string(),
                value: FieldValue::Text("x".repeat(400)),
            };
            let entry = HistoryEntry::engine("big", vec![command.clone()], vec![command]);
            history.push(entry, step as u64 * 1000, WINDOW);
        }
        assert!(history.memory_bytes() <= 2048);
        assert!(history.steps() < 8);
    }

    #[test]
    fn drop_stale_removes_invalid_entries_and_keeps_the_cursor() {
        let mut history = CommandHistory::new(10, usize::MAX);
        history.push(field_entry("keep one", 1.0), 0, WINDOW);
        history.push(
            HistoryEntry::engine(
                "stale",
                vec![EditorCommand::SetField {
                    entity: Entity::default(),
                    component: "Gone".to_string(),
                    field: "value".to_string(),
                    value: FieldValue::F32(2.0),
                }],
                Vec::new(),
            ),
            1000,
            WINDOW,
        );
        history.push(field_entry("keep two", 3.0), 2000, WINDOW);
        history.pop_undo(); // "keep two" is now unapplied.
        let dropped = history.drop_stale(|identifier| !identifier.starts_with("Gone"), 1);
        assert_eq!(dropped, 1);
        assert_eq!(history.steps(), 2);
        assert_eq!(history.undo_depth(), 1);
        assert_eq!(history.redo_depth(), 1);
        // The same registry stamp reports nothing the second time: the prune
        // rides one registry change, not one call.
        assert_eq!(history.drop_stale(|_| false, 1), 0);
        assert_eq!(history.drop_stale(|_| false, 2), 2);
    }

    #[test]
    fn remap_follows_a_restored_entity() {
        let old = Entity::default();
        let fresh = Entity::default();
        let mut history = CommandHistory::new(10, usize::MAX);
        history.push(
            HistoryEntry::engine(
                "on the restored entity",
                vec![EditorCommand::SetField {
                    entity: old,
                    component: "Demo".to_string(),
                    field: "strength".to_string(),
                    value: FieldValue::F32(1.0),
                }],
                vec![EditorCommand::SetField {
                    entity: old,
                    component: "Demo".to_string(),
                    field: "strength".to_string(),
                    value: FieldValue::F32(0.0),
                }],
            ),
            0,
            WINDOW,
        );
        history.remap_entity(old, fresh);
        let entry = history.pop_undo().expect("entry");
        let HistoryEntryKind::Engine { forward, inverse } = entry.entry else {
            panic!("engine entry expected");
        };
        for command in forward.iter().chain(inverse.iter()) {
            let EditorCommand::SetField { entity, .. } = command else {
                panic!("set field expected");
            };
            assert_eq!(*entity, fresh);
        }
    }

    #[test]
    fn selection_entries_round_trip() {
        let mut history = CommandHistory::new(10, usize::MAX);
        let before = Entity::default();
        let after = Entity::default();
        history.push(
            HistoryEntry::selection(Some(before), Some(after)),
            0,
            WINDOW,
        );
        let entry = history.pop_undo().expect("selection step");
        match entry.entry {
            HistoryEntryKind::Selection {
                before: b,
                after: a,
            } => {
                assert_eq!(b, Some(before));
                assert_eq!(a, Some(after));
            }
            HistoryEntryKind::Engine { .. } => panic!("selection entry expected"),
        }
    }

    #[test]
    fn limits_shrink_trims_existing_entries() {
        let mut history = CommandHistory::new(10, usize::MAX);
        for step in 0..6 {
            history.push(field_entry("step", step as f32), step as u64 * 1000, WINDOW);
        }
        let outcome = history.set_limits(2, usize::MAX);
        assert_eq!(history.steps(), 2);
        assert_eq!(outcome.trimmed, 4);
    }
}
