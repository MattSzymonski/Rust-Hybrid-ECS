//! The editor's command vocabulary and the metadata the history reads.
//!
//! # Responsibilities
//!
//! - Name every queued mutation ([`EditorCommand`]) - the one owned type the
//!   shell queues and the frame boundary applies.
//! - Answer what the history asks of a command: whether it records, which
//!   component and field identifiers it addresses, and which entity handle it
//!   names ([`EditorCommand::recordable`],
//!   [`EditorCommand::touched_identifiers`], [`EditorCommand::remap_entity`]).
//! - Classify each command into the family whose applier module owns it
//!   ([`CommandFamily`], [`EditorCommand::family`]).
//!
//! # Design
//!
//! The enum stays whole: it is the shell's queue format and crosses the reload
//! boundary, so splitting it per family would break that. Only the applier is
//! split, by [`CommandFamily`].

use pill_engine::Entity;

use super::*;

/// A queued editor mutation, flushed at the frame boundary.
#[derive(Clone, Debug, PartialEq)]
pub enum EditorCommand {
    /// Create an entity carrying the seeded components (empty vec = empty).
    CreateEntity {
        /// Components to attach; each must have a registered field layout.
        components: Vec<ComponentSeed>,
    },
    /// Destroy one entity.
    DestroyEntity {
        /// The entity to remove.
        entity: Entity,
    },
    /// Add a zero-initialised component to a live entity.
    AddComponent {
        /// The target entity.
        entity: Entity,
        /// Registered component type name.
        component: String,
    },
    /// Remove a component from a live entity (removing the last one destroys
    /// the entity).
    RemoveComponent {
        /// The target entity.
        entity: Entity,
        /// Registered component type name.
        component: String,
    },
    /// Re-create a destroyed entity from captured component images.
    ///
    /// The world assigns a fresh handle; the recording layer reports the
    /// `(previous, assigned)` pair so the history remaps older entries to it.
    /// Components without a field layout cannot be rebuilt and are absent
    /// from `components`: an entity carrying one restores without it.
    RestoreEntity {
        /// The handle the entity had before it was destroyed.
        previous: Entity,
        /// Component images to recreate, values included.
        components: Vec<ComponentSeed>,
    },
    /// Re-attach one component with captured field values, the inverse of
    /// removing it.
    RestoreComponent {
        /// The target entity.
        entity: Entity,
        /// Registered component type name.
        component: String,
        /// Field values to write over the zero-initialised image.
        fields: Vec<(String, FieldValue)>,
    },
    /// Write one field of a component.
    SetField {
        /// The target entity.
        entity: Entity,
        /// Registered component type name.
        component: String,
        /// Field name.
        field: String,
        /// New value.
        value: FieldValue,
    },
    /// Point an asset reference field at the asset `reference` names - a path
    /// relative to `res` or a 32-digit guid - or clear it with `None`.
    ///
    /// Resolved at the frame boundary, where the asset is loaded first when
    /// it is not loaded yet; it then writes like a [`Self::SetField`], whose
    /// inverse it records.
    SetFieldAsset {
        /// The target entity.
        entity: Entity,
        /// Registered component type name.
        component: String,
        /// Field name.
        field: String,
        /// The asset's path or guid, or `None` to clear the reference.
        reference: Option<String>,
    },
    /// Point an entity reference field at `reference`, or clear it with
    /// `None`.
    ///
    /// The handle is checked against the world when the batch is applied, so a
    /// reference to an entity that has gone by then is refused rather than
    /// written; a write records the [`Self::SetField`] that restores what the
    /// field held.
    SetFieldEntity {
        /// The target entity.
        entity: Entity,
        /// Registered component type name.
        component: String,
        /// Field name.
        field: String,
        /// The entity to reference, or `None` to clear the reference.
        reference: Option<Entity>,
    },
    /// Wait for the next scene click to name an entity reference field.
    ///
    /// The row's target button arms it and the scene click answers it; neither
    /// is an edit, so nothing is recorded until the click names an entity.
    ArmEntityPick {
        /// The entity the field belongs to.
        entity: Entity,
        /// Registered component type name.
        component: String,
        /// Field name.
        field: String,
    },
    /// Write one element of a fixed-size array field.
    SetArrayElement {
        /// The target entity.
        entity: Entity,
        /// Registered component type name.
        component: String,
        /// Array field name.
        field: String,
        /// Element index.
        index: usize,
        /// New element value.
        value: FieldValue,
    },
    /// Enable or disable a system by registration index.
    SetSystemEnabled {
        /// Registration index from the systems snapshot.
        index: usize,
        /// Desired state.
        enabled: bool,
    },
    /// Pause or resume the engine's system phase.
    SetPaused {
        /// Desired pause state.
        paused: bool,
    },
    /// Choose what the transform gizmo edits: the tool's mode,
    /// its space, or whether dragging snaps. Absent fields keep their value.
    SetGizmoTool {
        /// The mode to select.
        mode: Option<crate::gizmo::GizmoMode>,
        /// The space to select.
        space: Option<crate::gizmo::GizmoSpace>,
        /// The snapping toggle's new state.
        snap: Option<bool>,
    },
    /// Turn the scene grid on or off: the infinite ground the
    /// editor's view-bound surfaces draw.
    SetGrid {
        /// The grid's new state.
        enabled: bool,
    },
    /// Run exactly one system phase on the next frame, then pause again.
    StepOnce,
    /// Copy files from outside the project into the assets directory (a
    /// drag-and-drop import). Undo removes the copies; a file of
    /// the same name is overwritten.
    ImportFiles {
        /// Absolute source paths, as the OS reported them.
        sources: Vec<String>,
        /// The destination directory (the project's asset root).
        destination: String,
    },
    /// Delete the copies an import created (its inverse, and a redoable step
    /// of its own); it carries the origins so redo can copy them again.
    RemoveFiles {
        /// The created copies, absolute paths.
        paths: Vec<String>,
        /// The original source paths, the redo's sources.
        sources: Vec<String>,
        /// Where the copies live (the redo's destination).
        destination: String,
    },
    /// Create a standalone asset of `type_name` called `name` in `folder`,
    /// both relative to the asset directory.
    ///
    /// The file is the type's default document, written so an existing file is
    /// never replaced unless the dialog asked for that, then imported at once;
    /// its inverse is the deletion of the new file.
    CreateStandaloneAsset {
        /// The registered standalone type's name.
        type_name: String,
        /// The destination folder, `/`-separated and relative; empty for the
        /// asset directory's own root.
        folder: String,
        /// The name the user typed; the type's naming rules add the
        /// extension.
        name: String,
        /// What to do when the file is already there: `None` refuses (the
        /// dialog has not asked yet), `Some(true)` replaces it and keeps its
        /// guid, `Some(false)` replaces it with a fresh one.
        overwrite: Option<bool>,
    },
    /// Delete the asset file at `path`, relative to the asset directory,
    /// together with its `.meta` sidecar.
    ///
    /// The apply captures both files' bytes before deleting them; the
    /// inverse is the restore that puts them back, the sidecar's guid
    /// included, so the asset keeps its identity across an undo.
    DeleteAssetFile {
        /// The asset's path relative to the asset directory.
        path: String,
    },
    /// Restore the asset files a delete captured: the file's
    /// bytes, and the sidecar's when it had one. The inverse is the delete,
    /// so the pair round-trips; like the other restore commands this is
    /// never a recorded edit of its own.
    RestoreAssetFile {
        /// The asset's path relative to the asset directory.
        path: String,
        /// The file's bytes, exactly as they were.
        bytes: Vec<u8>,
        /// The `.meta` sidecar's bytes, when one existed.
        meta: Option<Vec<u8>>,
    },
    /// Move or rename the asset at `from` to `to`, both relative to the
    /// asset directory.
    ///
    /// The `.meta` sidecar travels with the file and the asset watcher
    /// follows the move, so the loaded asset keeps its handle and guid; the
    /// inverse moves it back.
    MoveAssetFile {
        /// The asset's current path relative to the asset directory.
        from: String,
        /// Where it goes, relative to the asset directory.
        to: String,
    },
    /// Write the `.meta` sidecar the sourced asset at `path` is missing, with
    /// the type's defaults and a fresh guid. A no-op - and
    /// so nothing recorded - where a sidecar exists; the inverse deletes
    /// the written sidecar.
    WriteAssetMeta {
        /// The asset's path relative to the asset directory.
        path: String,
    },
    /// Replace the import settings of the asset at `path` - or a standalone
    /// asset's own document - with `settings`. The inverse
    /// carries the previous value, which the apply reads before writing.
    SetAssetSettings {
        /// The asset's path relative to the asset directory.
        path: String,
        /// The new settings (or document) body.
        settings: serde_json::Value,
    },
    /// Set one field of one editor-editable resource.
    ///
    /// The engine keeps no resource reflection, so the resources this can
    /// name are curated in [`crate::resources::EDITABLE_RESOURCES`] and read
    /// and written field by field through [`crate::gizmo`]; the inverse
    /// carries the previous value.
    SetResourceField {
        /// The resource's stable name (its type path).
        resource: String,
        /// The field's name within the resource.
        field: String,
        /// The new value.
        value: FieldValue,
    },
    /// Set the shared selection. Consecutive selection changes
    /// merge into one history step through a coalesce key; the inverse
    /// carries the previous pair, so undo goes back one selection at a
    /// time.
    SetSelection {
        /// The selected entities, in selection order (the last is primary);
        /// empty selects none.
        entities: Vec<Entity>,
        /// The selected asset's path, when one is.
        asset: Option<String>,
    },
    /// Select one world resource for the Inspector.
    ///
    /// Its own command rather than a third field of [`EditorCommand::SetSelection`]:
    /// selecting a resource clears the entity and asset slots, and an undo of
    /// one selection must not resurrect an unrelated one.
    SelectResource {
        /// The resource's stable name, `None` to clear the selection.
        resource: Option<String>,
    },
    /// Restore all three selection slots as one recorded step.
    ///
    /// The inverse a resource selection applies: picking a resource clears the
    /// entity and asset slots, so undoing it has to bring them back rather than
    /// leave the Inspector pointing at nothing.
    RestoreSelection {
        /// The entity slots to restore.
        entities: Vec<Entity>,
        /// The asset slot to restore.
        asset: Option<String>,
        /// The resource slot to restore.
        resource: Option<String>,
    },
    /// Write preferences file values: one `(key, text)` pair per
    /// edited row, applied as a read-modify-write of the preferences
    /// document so keys other writers own survive. A pair whose text is
    /// blank writes nothing; the file's live reader applies the values
    /// (history limits, snapping steps) on its next look.
    SetPreferences {
        /// `(key, value)` pairs; keys are the file's own, dots nest.
        values: Vec<(String, String)>,
    },
    /// Start an empty scene: every entity goes, no file is open.
    ///
    /// A document action, not an edit: it is not recorded, and the history it
    /// would otherwise fill names entities this world no longer has.
    SceneNew,
    /// Open the scene at `path`, replacing the world's entities.
    SceneOpen {
        /// The scene's path below the asset directory.
        path: String,
    },
    /// Write the world to the open scene, keeping its guid.
    SceneSave,
    /// Write the world to `path`, taking over that file's guid when it exists
    /// and a fresh one otherwise.
    SceneSaveAs {
        /// The scene's path below the asset directory.
        path: String,
    },
}

/// What one applied command hands back to the recording layer.
pub(super) struct CommandOutcome {
    /// The command that reverses this one, when one exists.
    pub(super) inverse: Option<EditorCommand>,
    /// A `(previous, assigned)` handle pair when the command re-created an
    /// entity; the history remaps older entries to the new handle.
    pub(super) remap: Option<(Entity, Entity)>,
}

impl CommandOutcome {
    /// A successful command that produced an inverse.
    pub(super) fn inverse(inverse: EditorCommand) -> Self {
        Self {
            inverse: Some(inverse),
            remap: None,
        }
    }

    /// A successful command without an inverse (engine controls, restores).
    pub(super) fn plain() -> Self {
        Self {
            inverse: None,
            remap: None,
        }
    }
}

/// The result of [`EditorCommand::apply_recorded`].
#[derive(Debug, Default)]
pub struct RecordedApply {
    /// One `(index, message)` per failed command.
    pub failures: Vec<(usize, String)>,
    /// Inverses of the successful edits, in undo order (reverse application
    /// order).
    pub inverses: Vec<EditorCommand>,
    /// True when every command succeeded and produced an inverse; only then
    /// may the batch be recorded as one undo step.
    pub fully_invertible: bool,
    /// `(previous, assigned)` handle pairs from entity restores.
    pub remaps: Vec<(Entity, Entity)>,
}

/// The family a command belongs to: the applier module that owns its arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandFamily {
    /// Undoable world edits: fields, array elements, entity and component
    /// lifecycle, resource fields.
    World,
    /// Engine controls: pause, step, the gizmo tool, the grid, a system toggle.
    Engine,
    /// Selection changes and the request that arms the next scene click.
    Selection,
    /// Scene documents and preferences.
    Document,
    /// Asset-file I/O and its rollbacks.
    Asset,
}

impl EditorCommand {
    /// Whether this command is an undoable edit.
    ///
    /// Engine controls (pause, step) and the restore commands themselves are
    /// not edits: they never enter the history. A system toggle is an edit, so
    /// it records. Arming an entity pick is not an edit either - it waits for
    /// the next scene click, and the click's own answer is the recorded step.
    ///
    /// The two readers that make this predicate load-bearing agree on that
    /// meaning: a command enters the history only when it records, and a
    /// command that applied marks the document unsaved only when it records.
    /// A future command that must dirty the document without being undoable
    /// needs its own predicate, not another arm here.
    pub fn recordable(&self) -> bool {
        matches!(
            self,
            EditorCommand::CreateEntity { .. }
                | EditorCommand::DestroyEntity { .. }
                | EditorCommand::AddComponent { .. }
                | EditorCommand::RemoveComponent { .. }
                | EditorCommand::SetField { .. }
                | EditorCommand::SetFieldAsset { .. }
                | EditorCommand::SetFieldEntity { .. }
                | EditorCommand::SetArrayElement { .. }
                | EditorCommand::SetSystemEnabled { .. }
                | EditorCommand::ImportFiles { .. }
                | EditorCommand::RemoveFiles { .. }
                | EditorCommand::CreateStandaloneAsset { .. }
                | EditorCommand::DeleteAssetFile { .. }
                | EditorCommand::MoveAssetFile { .. }
                | EditorCommand::WriteAssetMeta { .. }
                | EditorCommand::SetAssetSettings { .. }
                | EditorCommand::SetResourceField { .. }
                | EditorCommand::SetSelection { .. }
                | EditorCommand::SelectResource { .. }
                | EditorCommand::RestoreSelection { .. }
        )
    }

    /// Component and field identifiers this command addresses.
    ///
    /// Field identifiers are `"component.field"`. The history keeps them per
    /// entry so a reload can drop entries whose types or fields are gone.
    pub fn touched_identifiers(&self) -> Vec<String> {
        let mut identifiers = Vec::new();
        match self {
            EditorCommand::SetField {
                component, field, ..
            }
            | EditorCommand::SetFieldAsset {
                component, field, ..
            }
            | EditorCommand::SetFieldEntity {
                component, field, ..
            }
            | EditorCommand::ArmEntityPick {
                component, field, ..
            }
            | EditorCommand::SetArrayElement {
                component, field, ..
            } => {
                identifiers.push(component.clone());
                identifiers.push(format!("{component}.{field}"));
            }
            EditorCommand::AddComponent { component, .. }
            | EditorCommand::RemoveComponent { component, .. }
            | EditorCommand::RestoreComponent { component, .. } => {
                identifiers.push(component.clone());
            }
            EditorCommand::CreateEntity { components }
            | EditorCommand::RestoreEntity { components, .. } => {
                for seed in components {
                    identifiers.push(seed.type_name.clone());
                    for (field, _) in &seed.fields {
                        identifiers.push(format!("{}.{}", seed.type_name, field));
                    }
                }
            }
            EditorCommand::DestroyEntity { .. }
            | EditorCommand::SetSystemEnabled { .. }
            | EditorCommand::SetPaused { .. }
            | EditorCommand::SetGizmoTool { .. }
            | EditorCommand::SetGrid { .. }
            | EditorCommand::StepOnce
            | EditorCommand::SetPreferences { .. }
            | EditorCommand::ImportFiles { .. }
            | EditorCommand::RemoveFiles { .. }
            | EditorCommand::CreateStandaloneAsset { .. }
            | EditorCommand::DeleteAssetFile { .. }
            | EditorCommand::RestoreAssetFile { .. }
            | EditorCommand::MoveAssetFile { .. }
            | EditorCommand::WriteAssetMeta { .. }
            | EditorCommand::SetAssetSettings { .. }
            | EditorCommand::SetResourceField { .. }
            | EditorCommand::SetSelection { .. }
            | EditorCommand::SelectResource { .. }
            | EditorCommand::RestoreSelection { .. }
            | EditorCommand::SceneNew
            | EditorCommand::SceneOpen { .. }
            | EditorCommand::SceneSave
            | EditorCommand::SceneSaveAs { .. } => {}
        }
        identifiers
    }

    /// Replace one entity handle with another wherever this command names it.
    ///
    /// Used after an undo re-created a destroyed entity under a fresh handle:
    /// every remaining history entry must follow the entity to its new
    /// identity, or a later undo would target the dead handle.
    pub fn remap_entity(&mut self, previous: Entity, assigned: Entity) {
        match self {
            EditorCommand::DestroyEntity { entity }
            | EditorCommand::AddComponent { entity, .. }
            | EditorCommand::RemoveComponent { entity, .. }
            | EditorCommand::RestoreComponent { entity, .. }
            | EditorCommand::SetField { entity, .. }
            | EditorCommand::SetFieldAsset { entity, .. }
            | EditorCommand::SetFieldEntity { entity, .. }
            | EditorCommand::ArmEntityPick { entity, .. }
            | EditorCommand::SetArrayElement { entity, .. } => {
                if *entity == previous {
                    *entity = assigned;
                }
            }
            EditorCommand::RestoreEntity { previous: old, .. } => {
                if *old == previous {
                    *old = assigned;
                }
            }
            EditorCommand::SetSelection { entities, .. }
            | EditorCommand::RestoreSelection { entities, .. } => {
                for entity in entities.iter_mut() {
                    if *entity == previous {
                        *entity = assigned;
                    }
                }
            }
            EditorCommand::CreateEntity { .. }
            | EditorCommand::SetSystemEnabled { .. }
            | EditorCommand::SetPaused { .. }
            | EditorCommand::SetGizmoTool { .. }
            | EditorCommand::SetGrid { .. }
            | EditorCommand::StepOnce
            | EditorCommand::SetPreferences { .. }
            | EditorCommand::ImportFiles { .. }
            | EditorCommand::RemoveFiles { .. }
            | EditorCommand::CreateStandaloneAsset { .. }
            | EditorCommand::DeleteAssetFile { .. }
            | EditorCommand::RestoreAssetFile { .. }
            | EditorCommand::MoveAssetFile { .. }
            | EditorCommand::WriteAssetMeta { .. }
            | EditorCommand::SetAssetSettings { .. }
            | EditorCommand::SetResourceField { .. }
            | EditorCommand::SelectResource { .. }
            | EditorCommand::SceneNew
            | EditorCommand::SceneOpen { .. }
            | EditorCommand::SceneSave
            | EditorCommand::SceneSaveAs { .. } => {}
        }
    }

    /// The applier module that owns this command.
    ///
    /// The applier routes on this, so a new variant is classified here once
    /// instead of gaining an arm in every family. It is deliberately not the
    /// source of [`Self::recordable`] or [`Self::touched_identifiers`]: those
    /// answer per variant, and one family can disagree - a system toggle
    /// records while pause and step do not.
    pub fn family(&self) -> CommandFamily {
        match self {
            EditorCommand::CreateEntity { .. }
            | EditorCommand::DestroyEntity { .. }
            | EditorCommand::AddComponent { .. }
            | EditorCommand::RemoveComponent { .. }
            | EditorCommand::RestoreEntity { .. }
            | EditorCommand::RestoreComponent { .. }
            | EditorCommand::SetField { .. }
            | EditorCommand::SetFieldAsset { .. }
            | EditorCommand::SetFieldEntity { .. }
            | EditorCommand::SetArrayElement { .. }
            | EditorCommand::SetResourceField { .. } => CommandFamily::World,
            EditorCommand::SetSystemEnabled { .. }
            | EditorCommand::SetPaused { .. }
            | EditorCommand::SetGizmoTool { .. }
            | EditorCommand::SetGrid { .. }
            | EditorCommand::StepOnce => CommandFamily::Engine,
            EditorCommand::SetSelection { .. }
            | EditorCommand::SelectResource { .. }
            | EditorCommand::RestoreSelection { .. }
            | EditorCommand::ArmEntityPick { .. } => CommandFamily::Selection,
            EditorCommand::SetPreferences { .. }
            | EditorCommand::SceneNew
            | EditorCommand::SceneOpen { .. }
            | EditorCommand::SceneSave
            | EditorCommand::SceneSaveAs { .. } => CommandFamily::Document,
            EditorCommand::ImportFiles { .. }
            | EditorCommand::RemoveFiles { .. }
            | EditorCommand::CreateStandaloneAsset { .. }
            | EditorCommand::DeleteAssetFile { .. }
            | EditorCommand::RestoreAssetFile { .. }
            | EditorCommand::MoveAssetFile { .. }
            | EditorCommand::WriteAssetMeta { .. }
            | EditorCommand::SetAssetSettings { .. } => CommandFamily::Asset,
        }
    }
}
