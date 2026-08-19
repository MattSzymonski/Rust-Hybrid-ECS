//! Component persistence and schema migration for hot-reload.
//!
//! # Responsibilities
//!
//! - Snapshots all entity component data before a hot-reload using serde_json.
//! - Restores data after new component types are registered, matching old→new
//!   components by type name (not TypeId), so renamed/reshaped components are
//!   handled gracefully.
//! - Stores per-component-type serialize/deserialize/insert function pointers
//!   registered alongside each persistable component.
//!
//! # Design
//!
//! Uses **serde_json** (not bincode) because JSON is self-describing:
//! field names are embedded in the payload, so adding/removing fields does
//! not break deserialization.  New fields receive `Default::default()`,
//! removed fields are silently ignored.
//!
//! Each persistable component type registers three monomorphized functions:
//!
//! | Function        | Signature | Purpose |
//! |-----------------|-----------|---------|
//! | `serialize`     | `fn(&TraitTypeMap, index) → Vec<u8>` | Read concrete component from archetype column, JSON-encode it |
//! | `deserialize`   | `fn(&[u8]) → Option<Box<dyn Component>>` | Decode JSON bytes back into a component; returns None on schema mismatch |
//! | `insert_boxed`  | `fn(&mut TraitTypeMap, Box<dyn Component>)` | Downcast and push into the concrete VecStorage |
//!
//! These functions are monomorphized in the project DLL (where the concrete
//! types are defined).  They are stored as plain function pointers in the
//! engine's `World`, so replacing them on reload (via `HashMap::insert`)
//! does not call any destructors through old vtables — function pointers
//! are trivially overwritten.
//!
//! During snapshot: iterate every archetype, call `serialize` for each
//! (entity, component_type) pair.
//!
//! During restore: destroy all existing entities, then for each snapshot
//! entry, `deserialize` → `Option<Box<dyn Component>>`, call `insert_boxed`
//! into the target archetype's storage.

// Standard library
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

// External crates
use serde::{de::DeserializeOwned, Serialize};
use trait_type_map::{TraitAccessible, TraitTypeMap, VecFamily};

// Current crate
use crate::component::{Component, ComponentId};
use crate::entity::Entity;
use crate::error::PersistenceError;
use crate::resource::Resource;
use crate::world::World;

// =============================================================================
// Function Pointer Type Aliases
// =============================================================================

/// Serializes the component at `index` in the given storage map into a
/// JSON-encoded byte vector.
pub(crate) type SerializeComponentFn =
    fn(storage: &TraitTypeMap<dyn Component, VecFamily>, index: usize) -> Vec<u8>;

/// Deserializes JSON bytes back into a heap-allocated component.
///
/// Returns `None` if the schema changed incompatibly (e.g. a field type
/// changed from `f32` to `String`).  Simple additions/removals are handled
/// by serde's `default` / `ignore` behaviour.
pub(crate) type DeserializeComponentFn = fn(bytes: &[u8]) -> Option<Box<dyn Component>>;

/// Downcasts a `Box<dyn Component>` to its concrete type and pushes it
/// into the appropriate `VecStorage<T>` inside the storage map.
pub(crate) type InsertComponentFn =
    fn(storage: &mut TraitTypeMap<dyn Component, VecFamily>, component: Box<dyn Component>);

/// Serializes one registered persistable resource into JSON bytes.
///
/// Returns `None` when the resource type is registered but no instance is
/// currently inserted, which is a normal state rather than a failure.
pub(crate) type SerializeResourceFn = fn(world: &World) -> Option<Vec<u8>>;

/// Deserializes JSON bytes and re-inserts the resource into the world.
///
/// Returns `false` when the payload cannot be decoded with the current schema,
/// leaving whatever instance `project_init` already created in place.
pub(crate) type RestoreResourceFn = fn(world: &mut World, bytes: &[u8]) -> bool;

// =============================================================================
// ComponentSnapshot
// =============================================================================

/// Captured component data for all entities at a point in time.
///
/// Used to preserve project state across hot-reloads.  Components are
/// matched by type **name** (a string like `"project::Position"`), not by
/// `TypeId`, so schema changes (added/removed fields) are handled by
/// serde's default-value / ignore-unknown behaviour.
///
/// # Examples
///
/// ```
/// use pill_engine::ComponentSnapshot;
///
/// let snapshot = ComponentSnapshot {
///     entries: vec![vec![("project::Position".to_string(), b"{}".to_vec())]],
///     ..Default::default()
/// };
/// assert_eq!(snapshot.entity_count(), 1);
/// ```
#[derive(Debug, Default)]
pub struct ComponentSnapshot {
    /// Each entry represents one entity to recreate.
    /// Inner vec: list of `(component_type_name, json_bytes)`.
    pub entries: Vec<Vec<(String, Vec<u8>)>>,
    /// Handle each entry was captured from, parallel to `entries`.
    ///
    /// Empty when the snapshot carries no identities, in which case restore
    /// falls back to allocating fresh handles. Any other length mismatch is
    /// treated the same way, so a truncated payload degrades instead of
    /// pairing components with the wrong identity.
    pub entity_ids: Vec<SnapshotEntityId>,
}

/// Captured identity of one snapshotted entity.
///
/// Restoring the exact `(id, generation)` pair keeps entity handles stored
/// inside components, resources, and project state valid across a reload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SnapshotEntityId {
    /// Slot identifier the entity occupied.
    pub id: u64,
    /// Generation counter that disambiguated recycled slots.
    pub generation: u32,
}

/// Snapshot of one persistable component type registration.
///
/// Captured before reload so the host can compare old and new schemas and
/// selectively migrate only changed component types.
#[derive(Clone, Copy)]
pub struct PersistTypeMetadata {
    /// Runtime component identifier used by archetype columns.
    pub component_id: ComponentId,
    /// Component schema hash derived from default JSON shape + size.
    pub schema_hash: u64,
    /// Old serializer function pointer that can read old column memory.
    pub serializer: SerializeComponentFn,
}

/// Lightweight manifest entry for current persistable component registrations.
///
/// Produced by [`World::persist_type_manifest`] so the host can compare pre-
/// and post-reload registrations and decide which types need migration.
#[derive(Clone)]
pub struct PersistTypeManifestEntry {
    /// Fully-qualified Rust type name.
    pub type_name: String,
    /// Runtime component identifier used by archetype columns.
    pub component_id: ComponentId,
    /// Component schema hash derived from default JSON shape + size.
    pub schema_hash: u64,
}

/// Result of selective migration.
///
/// Aggregates how many component types and entities were migrated by
/// [`World::migrate_changed_persistable_components`], plus the names of the
/// types that could not be migrated selectively.
#[derive(Debug, Default)]
pub struct SelectiveMigrationReport {
    /// Number of component types that were migrated.
    pub migrated_type_count: usize,
    /// Number of entities touched by selective migration.
    pub migrated_entity_count: usize,
    /// Component type names that could not be migrated selectively.
    pub skipped_type_names: Vec<String>,
}

/// Result of restoring captured resources into a world.
///
/// A skipped resource is not a failure: the type may have been removed from
/// the project, or its schema may have changed incompatibly. In both cases the
/// instance created by `project_init` stays in place.
#[derive(Debug, Default)]
pub struct ResourceRestoreReport {
    /// Number of resources decoded and re-inserted.
    pub restored_count: usize,
    /// Resource type names whose captured payload could not be restored.
    pub skipped_type_names: Vec<String>,
}

impl ComponentSnapshot {
    /// Number of entity snapshots stored.
    pub fn entity_count(&self) -> usize {
        self.entries.len()
    }
}

// =============================================================================
// World — Persistable Component Registration
// =============================================================================

impl World {
    /// Register a component type that supports persistence and schema migration.
    ///
    /// In addition to the normal component registration (bit index, storage
    /// factory, copier), this stores serialize/deserialize/insert function
    /// pointers so the engine can snapshot and restore this component type
    /// during hot-reload.
    ///
    /// The type `T` must implement `serde::Serialize + serde::DeserializeOwned`
    /// so that JSON can round-trip its data.  When the struct shape changes
    /// between reloads, serde matches fields by **name** — new fields get
    /// `Default::default()`, removed fields are silently ignored.
    pub fn register_persistable_component<T>(&mut self)
    where
        T: Component
            + TraitAccessible<dyn Component>
            + Clone
            + Serialize
            + DeserializeOwned
            + Default
            + 'static,
    {
        // Step 1: Perform the standard component registration (bit index,
        // storage factory, copier).
        self.register_component::<T>();

        let component_id = ComponentId::of::<T>();
        let type_name = std::any::type_name::<T>().to_string();

        // Step 2: Purge stale persist entries left over from previous
        // registrations of the same type name.  This handles the case where
        // a component struct is changed and then changed back — the compiler
        // may assign the same TypeId, but old entries from intermediate
        // shapes still pollute the persist maps.
        let stale_ids: Vec<ComponentId> = self
            .component_registry
            .registered_components()
            .filter(|(_, _, name)| *name == type_name)
            .map(|(id, _, _)| id)
            .filter(|id| *id != component_id)
            .collect();
        for stale_id in &stale_ids {
            self.persist_serializers.remove(stale_id);
            self.persist_inserters.remove(stale_id);
        }
        // Also clear the deserializer for this name — it will be
        // re-inserted below with the new function.
        self.persist_deserializers.remove(&type_name);

        // Step 3: Store the fresh monomorphized serialize, deserialize, and
        // insert function pointers plus the schema hash for the new shape.
        self.persist_serializers.insert(
            component_id,
            serialize_component::<T> as SerializeComponentFn,
        );

        self.persist_deserializers.insert(
            type_name.clone(),
            deserialize_component::<T> as DeserializeComponentFn,
        );

        self.persist_inserters.insert(
            component_id,
            insert_boxed_component::<T> as InsertComponentFn,
        );

        self.persist_schema_hashes
            .insert(type_name, calculate_schema_hash::<T>());
    }

    /// Register a resource type that survives engine and project reloads.
    ///
    /// Components live in archetype columns and are matched by type name;
    /// resources are singletons stored by `TypeId`, so they need their own
    /// registration to become part of a snapshot. A resource that is not
    /// registered here is simply recreated by `project_init` after a reload,
    /// which is the right behaviour for anything holding non-serializable
    /// state such as an `Instant`, a file handle, or a GPU resource.
    ///
    /// Registrations are keyed by type name, so re-registering the same
    /// resource from a newly loaded module replaces the previous entry rather
    /// than leaving a stale function pointer behind.
    ///
    /// # Examples
    ///
    /// ```
    /// # use pill_engine::{Resource, World};
    /// # use serde::{Deserialize, Serialize};
    /// #[derive(Serialize, Deserialize)]
    /// struct Score(u32);
    /// impl Resource for Score {}
    ///
    /// let mut world = World::new();
    /// world.register_persistable_resource::<Score>();
    /// world.insert_resource(Score(42));
    ///
    /// let captured = world.snapshot_resources();
    /// assert_eq!(captured.len(), 1);
    /// ```
    pub fn register_persistable_resource<T>(&mut self)
    where
        T: Resource + Serialize + DeserializeOwned + 'static,
    {
        let type_name = std::any::type_name::<T>().to_string();
        self.persist_resource_serializers.insert(
            type_name.clone(),
            serialize_resource::<T> as SerializeResourceFn,
        );
        self.persist_resource_restorers
            .insert(type_name, restore_resource::<T> as RestoreResourceFn);
    }
}

// =============================================================================
// World - Resource Snapshot and Restore
// =============================================================================

impl World {
    /// Capture every registered persistable resource as JSON bytes.
    ///
    /// Registered types with no inserted instance are omitted rather than
    /// captured as null, so restoring never resurrects a resource the project
    /// deliberately removed. The result is sorted by type name to keep
    /// captured payloads byte-stable across runs.
    pub fn snapshot_resources(&self) -> Vec<(String, Vec<u8>)> {
        let mut captured: Vec<(String, Vec<u8>)> = self
            .persist_resource_serializers
            .iter()
            .filter_map(|(type_name, serialize)| {
                serialize(self).map(|bytes| (type_name.clone(), bytes))
            })
            .collect();

        captured.sort_by(|left, right| left.0.cmp(&right.0));
        println!(
            "[persistence] Captured {} persistable resource(s) out of {} registered type(s)",
            captured.len(),
            self.persist_resource_serializers.len(),
        );
        captured
    }

    /// Re-insert captured resources, matching them by type name.
    ///
    /// Types the current project no longer registers are skipped, as are
    /// payloads that no longer decode under the current schema. Both leave the
    /// instance `project_init` already created untouched, which is the same
    /// degradation rule persistable components follow.
    pub fn restore_resources(&mut self, resources: &[(String, Vec<u8>)]) -> ResourceRestoreReport {
        let mut report = ResourceRestoreReport::default();

        for (type_name, bytes) in resources {
            let Some(&restore) = self.persist_resource_restorers.get(type_name) else {
                println!("[persistence]   resource '{type_name}' -> SKIP (type not registered)");
                report.skipped_type_names.push(type_name.clone());
                continue;
            };

            if restore(self, bytes) {
                report.restored_count += 1;
            } else {
                println!("[persistence]   resource '{type_name}' -> SKIP (deserialize failed)");
                report.skipped_type_names.push(type_name.clone());
            }
        }

        println!(
            "[persistence] Resource restore complete: {} restored, {} skipped",
            report.restored_count,
            report.skipped_type_names.len(),
        );
        report
    }
}

// =============================================================================
// World — Snapshot and Restore
// =============================================================================

impl World {
    /// Capture all persistable component data from every living entity.
    ///
    /// Iterates every archetype and every entity index, calling the
    /// registered `serialize` function for each persistable component type.
    /// Non-persistable components (those registered with plain
    /// `register_component`) are silently skipped.
    ///
    /// # Panics
    ///
    /// Panics if a persistable component's JSON serialization fails (should
    /// never happen for valid component types).
    pub fn snapshot_components(&self) -> ComponentSnapshot {
        let total_entities = self.entity_locations.len();
        let total_archetypes = self.archetypes.len();
        let persistable_type_count = self.persist_serializers.len();

        println!(
            "[persistence] Snapshotting {} entities across {} archetypes ({} persistable component types)...",
            total_entities, total_archetypes, persistable_type_count,
        );

        let mut entries: Vec<Vec<(String, Vec<u8>)>> = Vec::with_capacity(total_entities);
        let mut entity_ids: Vec<SnapshotEntityId> = Vec::with_capacity(total_entities);
        let mut total_components_serialized: usize = 0;
        let mut skipped_non_persistable: usize = 0;

        // Serialize every persistable component across all archetypes
        // and entities; non-persistable components are counted and skipped.
        for archetype in self.archetypes.values() {
            let entity_count = archetype.entities.len();
            if entity_count == 0 {
                continue;
            }

            for entity_index in 0..entity_count {
                let mut component_data: Vec<(String, Vec<u8>)> = Vec::new();

                for component_id in &archetype.component_types {
                    if let Some(&serialize_fn) = self.persist_serializers.get(component_id) {
                        let type_name = self
                            .component_registry
                            .get_name(component_id)
                            .unwrap_or("?")
                            .to_string();
                        let bytes = serialize_fn(&archetype.component_storages, entity_index);
                        let byte_len = bytes.len();
                        let name_for_log = type_name.clone();
                        component_data.push((type_name, bytes));
                        total_components_serialized += 1;

                        if total_components_serialized <= 5 {
                            println!(
                                "[persistence]   snapshot '{}' → {} bytes",
                                name_for_log, byte_len,
                            );
                        }
                    } else {
                        skipped_non_persistable += 1;
                    }
                }

                if !component_data.is_empty() {
                    // Record the handle alongside its components so restore can
                    // reinstate the exact identity instead of a fresh one.
                    let entity = archetype.entities[entity_index];
                    entity_ids.push(SnapshotEntityId {
                        id: entity.id(),
                        generation: entity.generation(),
                    });
                    entries.push(component_data);
                }
            }
        }

        println!(
            "[persistence] Snapshot complete: {} entities, {} components serialized ({} non-persistable skipped)",
            entries.len(),
            total_components_serialized,
            skipped_non_persistable,
        );

        ComponentSnapshot {
            entries,
            entity_ids,
        }
    }

    /// Destroy all entities and recreate them from a snapshot.
    ///
    /// This is called after a hot-reload: the old component types have been
    /// replaced by new ones (with potentially different `TypeId`s), but the
    /// snapshot still holds data keyed by type **name**.  Matching is done
    /// by name — components whose type name is not found in the new
    /// registration are silently dropped (the component type was removed).
    ///
    /// Deserialization uses JSON, so field additions (serde fills
    /// `Default::default()`) and field removals (serde ignores unknown
    /// keys) are handled gracefully.  Incompatible changes (e.g. changing
    /// a field's type from `f32` to `String`) cause that component to be
    /// skipped with a warning.
    pub fn restore_from_snapshot(&mut self, snapshot: &ComponentSnapshot) {
        let snapshot_entity_count = snapshot.entries.len();
        println!(
            "[persistence] Restoring {} entities from snapshot...",
            snapshot_entity_count,
        );

        // Step 1: Destroy all existing entities so stale TypeIds cannot
        // alias new registrations.
        let all_entity_ids: Vec<Entity> = self.entity_locations.keys().copied().collect();
        let destroyed_count = all_entity_ids.len();
        for entity in all_entity_ids {
            let _ = self.destroy_entity(entity);
        }
        println!(
            "[persistence]   Destroyed {} old entities (stale TypeIds)",
            destroyed_count,
        );

        // Step 2: Clear the allocator state that would otherwise block the
        // captured identities. Snapshots without identities (or with a
        // truncated identity list) fall back to fresh handles.
        let preserve_entity_ids = snapshot.entity_ids.len() == snapshot.entries.len();
        let mut reserved_generations = if preserve_entity_ids {
            self.reserve_explicit_entity_ids(&snapshot.entity_ids)
        } else {
            if !snapshot.entity_ids.is_empty() {
                println!(
                    "[persistence]   Ignoring {} captured entity ids: the identity list does not match the {} snapshot entries",
                    snapshot.entity_ids.len(),
                    snapshot.entries.len(),
                );
            }
            HashMap::new()
        };
        let mut preserved_entity_id_count: usize = 0;
        let mut rekeyed_entity_id_count: usize = 0;

        // Step 3: Recreate entities from snapshot data.
        let mut restored_entity_count: usize = 0;
        let mut restored_component_total: usize = 0;
        let mut skipped_type_removed: usize = 0;
        let mut skipped_deser_fail: usize = 0;
        let mut skipped_no_inserter: usize = 0;

        for (entry_idx, component_set) in snapshot.entries.iter().enumerate() {
            let mut restored_components: Vec<(ComponentId, Box<dyn Component>)> = Vec::new();
            let mut restored_component_ids: Vec<ComponentId> = Vec::new();

            for (type_name, bytes) in component_set {
                if let Some(&deserialize_fn) = self.persist_deserializers.get(type_name) {
                    if let Some(component) = deserialize_fn(bytes) {
                        if let Some(component_id) = self.resolve_component_id_by_name(type_name) {
                            if entry_idx < 3 {
                                println!(
                                    "[persistence]   restore '{}' → ok ({} bytes)",
                                    type_name,
                                    bytes.len(),
                                );
                            }
                            restored_components.push((component_id, component));
                            restored_component_ids.push(component_id);
                        } else {
                            skipped_no_inserter += 1;
                            if entry_idx < 3 {
                                println!(
                                    "[persistence]   restore '{}' → SKIP (no inserter)",
                                    type_name,
                                );
                            }
                        }
                    } else {
                        skipped_deser_fail += 1;
                        println!(
                            "[persistence]   restore '{}' → SKIP (deserialize failed)",
                            type_name,
                        );
                    }
                } else {
                    skipped_type_removed += 1;
                    if entry_idx < 3 {
                        println!(
                            "[persistence]   restore '{}' → SKIP (type removed)",
                            type_name,
                        );
                    }
                }
            }

            if restored_components.is_empty() {
                continue;
            }

            // Place the entity in the archetype that owns its exact set of
            // restored component types, inserting each restored component and
            // seeding change ticks.
            let entity = match snapshot.entity_ids.get(entry_idx) {
                Some(&captured) if preserve_entity_ids => {
                    let restored = resolve_explicit_entity(captured, &mut reserved_generations);
                    if restored.generation() == captured.generation {
                        preserved_entity_id_count += 1;
                    } else {
                        rekeyed_entity_id_count += 1;
                    }
                    restored
                }
                _ => self.allocate_entity(),
            };
            restored_component_ids.sort();

            let archetype_id = self.get_or_create_archetype(restored_component_ids.clone());
            let current_tick = crate::component::Tick::new(self.change_tick);

            {
                let archetype = self
                    .archetypes
                    .get_mut(&archetype_id)
                    .expect("archetype must exist after get_or_create_archetype");
                let index = archetype.entities.len();
                archetype.entities.push(entity);

                for (component_id, component) in restored_components {
                    if let Some(&insert_fn) = self.persist_inserters.get(&component_id) {
                        insert_fn(&mut archetype.component_storages, component);
                        restored_component_total += 1;
                    }
                }

                for &component_id in &archetype.component_types {
                    archetype
                        .component_ticks
                        .entry(component_id)
                        .or_default()
                        .push(crate::component::ComponentTicks::new(current_tick));
                }

                self.entity_locations.insert(
                    entity,
                    crate::world::EntityLocation {
                        archetype_id,
                        index_in_archetype: index,
                    },
                );
            }

            restored_entity_count += 1;
        }

        println!(
            "[persistence] Restore complete: {} entities, {} components inserted",
            restored_entity_count, restored_component_total,
        );
        if preserve_entity_ids {
            println!(
                "[persistence]   Entity ids: {} preserved exactly, {} re-keyed after a collision",
                preserved_entity_id_count, rekeyed_entity_id_count,
            );
        }
        if skipped_type_removed > 0 {
            println!(
                "[persistence]   {} components skipped (type removed from project)",
                skipped_type_removed,
            );
        }
        if skipped_deser_fail > 0 {
            println!(
                "[persistence]   {} components skipped (deserialization failed)",
                skipped_deser_fail,
            );
        }
        if skipped_no_inserter > 0 {
            println!(
                "[persistence]   {} components skipped (no inserter for TypeId)",
                skipped_no_inserter,
            );
        }
        println!(
            "[persistence]   World now has {} entities in {} archetypes",
            self.entity_locations.len(),
            self.archetypes.len(),
        );
    }

    /// Look up a [`ComponentId`] by the component's type name string,
    /// returning the most recently registered match (highest bit index).
    ///
    /// After multiple hot-reloads the component registry accumulates one
    /// entry per reload (each with a different `TypeId` but the same type
    /// name).  The entry with the highest bit is the one registered most
    /// recently and is the only one present in `persist_inserters`.
    fn resolve_component_id_by_name(&self, type_name: &str) -> Option<ComponentId> {
        self.component_registry
            .registered_components()
            .filter(|(_, _, name)| *name == type_name)
            .filter(|(id, _, _)| self.persist_inserters.contains_key(id))
            .max_by_key(|(_, bit, _)| *bit)
            .map(|(id, _, _)| id)
    }

    /// Return current persistable component manifest.
    pub fn persist_type_manifest(&self) -> Vec<PersistTypeManifestEntry> {
        let mut entries: Vec<PersistTypeManifestEntry> = self
            .persist_schema_hashes
            .iter()
            .filter_map(|(type_name, schema_hash)| {
                self.resolve_component_id_by_name(type_name)
                    .map(|component_id| PersistTypeManifestEntry {
                        type_name: type_name.clone(),
                        component_id,
                        schema_hash: *schema_hash,
                    })
            })
            .collect();

        entries.sort_by(|left, right| left.type_name.cmp(&right.type_name));
        entries
    }

    /// Capture old persistable component metadata before hot-reload.
    pub fn capture_persist_type_metadata(&self) -> HashMap<String, PersistTypeMetadata> {
        let mut metadata_by_name: HashMap<String, PersistTypeMetadata> = HashMap::new();

        for manifest_entry in self.persist_type_manifest() {
            if let Some(&serializer) = self.persist_serializers.get(&manifest_entry.component_id) {
                metadata_by_name.insert(
                    manifest_entry.type_name,
                    PersistTypeMetadata {
                        component_id: manifest_entry.component_id,
                        schema_hash: manifest_entry.schema_hash,
                        serializer,
                    },
                );
            }
        }

        metadata_by_name
    }

    /// Migrate only changed persistable components.
    ///
    /// For each changed type name, this uses the old serializer (captured before
    /// reload) and the new deserializer/inserter (registered by new project_init)
    /// to rewrite only the affected component columns.
    pub fn migrate_changed_persistable_components(
        &mut self,
        previous_metadata_by_name: &HashMap<String, PersistTypeMetadata>,
        changed_type_names: &HashSet<String>,
    ) -> SelectiveMigrationReport {
        let mut report = SelectiveMigrationReport::default();

        let mut sorted_changed_type_names: Vec<String> =
            changed_type_names.iter().cloned().collect();
        sorted_changed_type_names.sort();

        println!(
            "[persistence] Selective migration starting for {} component type(s)...",
            sorted_changed_type_names.len(),
        );

        for type_name in &sorted_changed_type_names {
            let Some(previous_metadata) = previous_metadata_by_name.get(type_name) else {
                println!(
                    "[persistence]   '{}' -> SKIP (missing previous metadata)",
                    type_name,
                );
                report.skipped_type_names.push(type_name.clone());
                continue;
            };

            let current_schema_hash = self
                .persist_schema_hashes
                .get(type_name)
                .copied()
                .unwrap_or(0);
            println!(
                "[persistence]   '{}' -> migrating (schema {} -> {})",
                type_name, previous_metadata.schema_hash, current_schema_hash,
            );

            match self.migrate_single_component_type(type_name, previous_metadata) {
                Ok(migrated_entity_count_for_type) => {
                    println!(
                        "[persistence]   '{}' -> OK ({} entities)",
                        type_name, migrated_entity_count_for_type,
                    );
                    report.migrated_type_count += 1;
                    report.migrated_entity_count += migrated_entity_count_for_type;
                }
                Err(error) => {
                    println!("[persistence]   '{type_name}' -> SKIP ({error})",);
                    report.skipped_type_names.push(type_name.clone());
                }
            }
        }

        println!(
            "[persistence] Selective migration finished: {} type(s) migrated, {} entities touched, {} type(s) skipped.",
            report.migrated_type_count,
            report.migrated_entity_count,
            report.skipped_type_names.len(),
        );

        report
    }

    /// Migrate one persistable component type from its previous registration
    /// to the current one.
    ///
    /// Chooses between an in-place column swap (when the [`ComponentId`] is
    /// unchanged) and a full archetype remap (when the component id changed
    /// after a reload).
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError::ComponentTypeUnregistered`] when the type
    /// name is no longer registered, [`PersistenceError::DeserializerMissing`]
    /// when no deserializer is registered for the type name, and
    /// [`PersistenceError::InserterMissing`] when no inserter is registered
    /// for the resolved component id.  Errors from the underlying in-place or
    /// cross-archetype migration propagate unchanged.
    fn migrate_single_component_type(
        &mut self,
        type_name: &str,
        previous_metadata: &PersistTypeMetadata,
    ) -> Result<usize, PersistenceError> {
        let Some(new_component_id) = self.resolve_component_id_by_name(type_name) else {
            return Err(PersistenceError::ComponentTypeUnregistered {
                type_name: type_name.to_string(),
            });
        };

        let Some(&deserialize_component) = self.persist_deserializers.get(type_name) else {
            return Err(PersistenceError::DeserializerMissing {
                type_name: type_name.to_string(),
            });
        };

        let Some(&insert_component) = self.persist_inserters.get(&new_component_id) else {
            return Err(PersistenceError::InserterMissing {
                type_name: type_name.to_string(),
            });
        };

        if previous_metadata.component_id == new_component_id {
            println!(
                "[persistence]     strategy: in-place column swap for '{}'",
                type_name,
            );
            self.migrate_component_column_in_place(
                previous_metadata.component_id,
                previous_metadata.serializer,
                deserialize_component,
                insert_component,
            )
        } else {
            println!(
                "[persistence]     strategy: archetype remap for '{}' (component id changed)",
                type_name,
            );
            self.migrate_component_across_archetypes(
                previous_metadata.component_id,
                new_component_id,
                previous_metadata.serializer,
                deserialize_component,
                insert_component,
            )
        }
    }

    /// Rewrite one component column in place inside every archetype that
    /// contains the old component id.
    ///
    /// Serializes each old value, deserializes it with the new schema
    /// (falling back to `{}` when the snapshot bytes no longer parse),
    /// removes the old storage column, recreates it through the registered
    /// storage factory, and inserts the migrated values.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError::DeserializationFailed`] when a value cannot
    /// be decoded with either the snapshot bytes or `{}`,
    /// [`PersistenceError::StorageRemovalFailed`] when the old storage column
    /// cannot be removed, [`PersistenceError::StorageFactoryMissing`] when no
    /// storage factory is registered, and
    /// [`PersistenceError::NativeStorageExpected`] when the registered
    /// factory is not a native storage factory.
    fn migrate_component_column_in_place(
        &mut self,
        component_id: ComponentId,
        serialize_old_component: SerializeComponentFn,
        deserialize_new_component: DeserializeComponentFn,
        insert_new_component: InsertComponentFn,
    ) -> Result<usize, PersistenceError> {
        // Step 1: Collect the archetypes whose columns contain the old
        // component id.
        let archetype_ids: Vec<_> = self
            .archetypes
            .iter()
            .filter(|(_, archetype)| archetype.component_types.contains(&component_id))
            .map(|(archetype_id, _)| *archetype_id)
            .collect();

        let mut migrated_entity_count: usize = 0;

        for archetype_id in archetype_ids {
            // Step 2: Serialize every old value before the storage column is
            // removed.
            let serialized_components: Vec<Vec<u8>> = {
                let Some(archetype) = self.archetypes.get(&archetype_id) else {
                    continue;
                };

                (0..archetype.entities.len())
                    .map(|entity_index| {
                        serialize_old_component(&archetype.component_storages, entity_index)
                    })
                    .collect()
            };

            // Step 3: Decode each value with the new schema, falling back to
            // an empty `{}` object when the snapshot bytes no longer parse.
            let mut migrated_components: Vec<Box<dyn Component>> =
                Vec::with_capacity(serialized_components.len());

            for bytes in &serialized_components {
                let component = deserialize_new_component(bytes)
                    .or_else(|| deserialize_new_component(b"{}"))
                    .ok_or(PersistenceError::DeserializationFailed { component_id })?;
                migrated_components.push(component);
            }

            // Step 4: Swap the storage column: remove the old one, recreate it
            // through the registered storage factory, and insert the migrated
            // values.
            let Some(archetype) = self.archetypes.get_mut(&archetype_id) else {
                continue;
            };

            if archetype
                .component_storages
                .remove_trait_storage(
                    component_id
                        .native_type_id()
                        .expect("persisted components must have native Rust storage"),
                )
                .is_none()
            {
                return Err(PersistenceError::StorageRemovalFailed { component_id });
            }

            let Some(storage_factory) = self.storage_factories.get(&component_id) else {
                return Err(PersistenceError::StorageFactoryMissing { component_id });
            };
            let crate::archetype::StorageFactory::Native(storage_factory) = storage_factory else {
                return Err(PersistenceError::NativeStorageExpected { component_id });
            };
            storage_factory(&mut archetype.component_storages);

            for component in migrated_components {
                insert_new_component(&mut archetype.component_storages, component);
            }

            migrated_entity_count += serialized_components.len();
        }

        Ok(migrated_entity_count)
    }

    /// Move every entity from archetypes containing the old component id into
    /// archetypes containing the new component id.
    ///
    /// Serializes each old component value, deserializes it with the new
    /// schema (falling back to `{}`), copies the unchanged components through
    /// their registered copiers, and re-inserts the migrated component.  Used
    /// when a reload changes the [`ComponentId`] assigned to a type name.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError::DeserializationFailed`] when a migrated
    /// value cannot be decoded with either the snapshot bytes or `{}`,
    /// [`PersistenceError::DestinationArchetypeMissing`] when the destination
    /// archetype cannot be created, and [`PersistenceError::CopierMissing`]
    /// when an unchanged component has no registered copier.
    fn migrate_component_across_archetypes(
        &mut self,
        old_component_id: ComponentId,
        new_component_id: ComponentId,
        serialize_old_component: SerializeComponentFn,
        deserialize_new_component: DeserializeComponentFn,
        insert_new_component: InsertComponentFn,
    ) -> Result<usize, PersistenceError> {
        // Step 1: Locate every archetype that contains the old component id.
        let source_archetype_ids: Vec<_> = self
            .archetypes
            .iter()
            .filter(|(_, archetype)| archetype.component_types.contains(&old_component_id))
            .map(|(archetype_id, _)| *archetype_id)
            .collect();

        let mut migrated_entity_count: usize = 0;

        for source_archetype_id in source_archetype_ids {
            // Step 2: Remove the source archetype and compute its destination
            // component set with the old id replaced by the new one.
            let Some(source_archetype) = self.archetypes.remove(&source_archetype_id) else {
                continue;
            };

            let mut destination_component_ids: Vec<ComponentId> = source_archetype
                .component_types
                .iter()
                .map(|component_id| {
                    if *component_id == old_component_id {
                        new_component_id
                    } else {
                        *component_id
                    }
                })
                .collect();

            destination_component_ids.sort();
            destination_component_ids.dedup();

            let destination_archetype_id = self.get_or_create_archetype(destination_component_ids);

            // Step 3: Serialize the old component values and decode them with
            // the new schema, falling back to `{}`.
            let serialized_components: Vec<Vec<u8>> = (0..source_archetype.entities.len())
                .map(|entity_index| {
                    serialize_old_component(&source_archetype.component_storages, entity_index)
                })
                .collect();

            let mut migrated_components: Vec<Box<dyn Component>> =
                Vec::with_capacity(serialized_components.len());

            for bytes in &serialized_components {
                let component = deserialize_new_component(bytes)
                    .or_else(|| deserialize_new_component(b"{}"))
                    .ok_or(PersistenceError::DeserializationFailed {
                        component_id: old_component_id,
                    })?;
                migrated_components.push(component);
            }

            // Step 4: Move each entity into the destination archetype,
            // copying the unchanged components and inserting the migrated
            // one, then re-record its location and per-component ticks.
            let Some(destination_archetype) = self.archetypes.get_mut(&destination_archetype_id)
            else {
                return Err(PersistenceError::DestinationArchetypeMissing);
            };

            let current_tick = crate::component::Tick::new(self.change_tick);

            for (entity_index, component) in migrated_components.into_iter().enumerate() {
                let entity = source_archetype.entities[entity_index];
                let destination_index = destination_archetype.entities.len();
                destination_archetype.entities.push(entity);

                for source_component_id in &source_archetype.component_types {
                    if *source_component_id == old_component_id {
                        continue;
                    }

                    let Some(&copy_component) = self.component_copiers.get(source_component_id)
                    else {
                        return Err(PersistenceError::CopierMissing {
                            component_id: *source_component_id,
                        });
                    };

                    copy_component(
                        &source_archetype.component_storages,
                        &mut destination_archetype.component_storages,
                        entity_index,
                    );
                }

                insert_new_component(&mut destination_archetype.component_storages, component);

                for destination_component_id in &destination_archetype.component_types {
                    let tick = if *destination_component_id == new_component_id {
                        source_archetype
                            .component_ticks
                            .get(&old_component_id)
                            .and_then(|ticks| ticks.get(entity_index))
                            .copied()
                            .unwrap_or(crate::component::ComponentTicks::new(current_tick))
                    } else {
                        source_archetype
                            .component_ticks
                            .get(destination_component_id)
                            .and_then(|ticks| ticks.get(entity_index))
                            .copied()
                            .unwrap_or(crate::component::ComponentTicks::new(current_tick))
                    };

                    destination_archetype
                        .component_ticks
                        .entry(*destination_component_id)
                        .or_default()
                        .push(tick);
                }

                self.entity_locations.insert(
                    entity,
                    crate::world::EntityLocation {
                        archetype_id: destination_archetype_id,
                        index_in_archetype: destination_index,
                    },
                );
            }

            migrated_entity_count += source_archetype.entities.len();
        }

        // Step 5: Bump the archetype generation so cached query plans observe
        // the new archetype layout.
        self.archetype_generation = self.archetype_generation.wrapping_add(1);
        Ok(migrated_entity_count)
    }
}

// =============================================================================
// Per-Type Monomorphized Functions
// =============================================================================
//
// These generic functions are monomorphized once per concrete component type
// inside the project DLL.  They are stored as plain `fn` pointers in the engine's
// HashMaps, so replacing them on hot-reload simply overwrites the pointer —
// no destructors, no vtable calls, no DLL-unload issues.

/// Serialize a single component at `index` from storage into JSON bytes.
fn serialize_component<T>(storage: &TraitTypeMap<dyn Component, VecFamily>, index: usize) -> Vec<u8>
where
    T: Component + TraitAccessible<dyn Component> + Serialize,
{
    let typed_storage = storage.get_storage::<T>();
    let value: &T = typed_storage.get(index);
    serde_json::to_vec(value).expect("JSON serialization failed")
}

/// Deserialize JSON bytes into a heap-allocated component.
///
/// If the schema changed (e.g. a field was added), missing fields are
/// filled from `T::default()` by merging the default JSON with the
/// snapshot JSON before deserializing.  Unknown fields (removed in the
/// new schema) are silently ignored by serde.
///
/// Returns `None` only on truly incompatible changes (field type changed).
fn deserialize_component<T>(bytes: &[u8]) -> Option<Box<dyn Component>>
where
    T: Component + Serialize + DeserializeOwned + Default + 'static,
{
    // Step 1: Deserialize the snapshot bytes into a generic JSON Value.
    let snapshot_json: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| {
            eprintln!(
                "[persistence] Failed to parse JSON for '{}': {}",
                std::any::type_name::<T>(),
                error
            );
        })
        .ok()?;

    // Step 2: Serialize a default instance to JSON as the schema baseline
    // for fields missing from the snapshot.
    let default_instance = T::default();
    let default_json: serde_json::Value = serde_json::to_value(&default_instance)
        .map_err(|error| {
            eprintln!(
                "[persistence] Failed to serialize default for '{}': {}",
                std::any::type_name::<T>(),
                error
            );
        })
        .ok()?;

    // Step 3: Merge the defaults with the snapshot data; snapshot values
    // override defaults where both are present.
    let merged = merge_json(default_json, snapshot_json);

    match serde_json::from_value::<T>(merged) {
        Ok(value) => Some(Box::new(value)),
        Err(error) => {
            eprintln!(
                "[persistence] Failed to deserialize '{}': {}. Component data skipped.",
                std::any::type_name::<T>(),
                error
            );
            None
        }
    }
}

/// Deep-merge two JSON values: `base` provides defaults, `override_json`
/// provides the actual data.  Fields present in `override_json` take
/// precedence; fields only in `base` are kept as defaults.
fn merge_json(mut base: serde_json::Value, override_json: serde_json::Value) -> serde_json::Value {
    match (&mut base, override_json) {
        (serde_json::Value::Object(base_map), serde_json::Value::Object(override_map)) => {
            for (key, value) in override_map {
                match base_map.get_mut(&key) {
                    Some(base_val) => {
                        // Recursively merge nested objects.
                        let merged = merge_json(base_val.clone(), value);
                        *base_val = merged;
                    }
                    None => {
                        // Field in snapshot but not in default — keep it
                        // (handles fields that were removed from the struct).
                        base_map.insert(key, value);
                    }
                }
            }
            serde_json::Value::Object(base_map.clone())
        }
        // For non-object values (primitives, arrays), override wins.
        (_, ov) => ov,
    }
}

/// Build a normalized schema-shape JSON tree where values are replaced by
/// stable kind markers. This avoids false negatives from default value changes.
fn normalize_schema_shape(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Null => serde_json::Value::String("null".to_string()),
        serde_json::Value::Bool(_) => serde_json::Value::String("bool".to_string()),
        serde_json::Value::Number(_) => serde_json::Value::String("number".to_string()),
        serde_json::Value::String(_) => serde_json::Value::String("string".to_string()),
        serde_json::Value::Array(values) => {
            let normalized_values: Vec<serde_json::Value> =
                values.iter().map(normalize_schema_shape).collect();
            serde_json::Value::Array(normalized_values)
        }
        serde_json::Value::Object(map) => {
            let mut normalized_map = serde_json::Map::new();
            let mut sorted_keys: Vec<&String> = map.keys().collect();
            sorted_keys.sort();

            for key in sorted_keys {
                if let Some(entry_value) = map.get(key) {
                    normalized_map.insert(key.clone(), normalize_schema_shape(entry_value));
                }
            }

            serde_json::Value::Object(normalized_map)
        }
    }
}

/// Compute schema hash for one persistable component type.
fn calculate_schema_hash<T>() -> u64
where
    T: Component + Serialize + Default + 'static,
{
    // Step 1: Serialize a default instance and normalize it to stable kind
    // markers so default-value changes do not alter the schema hash.
    let default_value = T::default();
    let default_json = serde_json::to_value(default_value).unwrap_or(serde_json::Value::Null);
    let normalized_schema = normalize_schema_shape(&default_json);
    let normalized_schema_string = serde_json::to_string(&normalized_schema)
        .unwrap_or_else(|_| "<schema-serialization-failed>".to_string());

    // Step 2: Combine the type name, size, and normalized schema into a
    // stable 64-bit hash for comparing schemas across reloads.
    let mut hasher = DefaultHasher::new();
    std::any::type_name::<T>().hash(&mut hasher);
    std::mem::size_of::<T>().hash(&mut hasher);
    normalized_schema_string.hash(&mut hasher);
    hasher.finish()
}

/// Serialize the single instance of one persistable resource type.
///
/// Returns `None` when no instance is inserted, or when the value cannot be
/// encoded; both cases simply leave the resource out of the snapshot.
fn serialize_resource<T>(world: &World) -> Option<Vec<u8>>
where
    T: Resource + Serialize + 'static,
{
    let resource = world.get_resource::<T>()?;
    match serde_json::to_vec(resource) {
        Ok(bytes) => Some(bytes),
        Err(error) => {
            eprintln!(
                "[persistence] Failed to serialize resource '{}': {}",
                std::any::type_name::<T>(),
                error
            );
            None
        }
    }
}

/// Decode captured bytes and re-insert one persistable resource type.
///
/// Missing fields are filled from the payload's own JSON shape by serde's
/// `default` handling where the type provides it; a payload that cannot be
/// decoded at all is reported so the caller can keep the instance
/// `project_init` created.
fn restore_resource<T>(world: &mut World, bytes: &[u8]) -> bool
where
    T: Resource + DeserializeOwned + 'static,
{
    match serde_json::from_slice::<T>(bytes) {
        Ok(value) => {
            world.insert_resource(value);
            true
        }
        Err(error) => {
            eprintln!(
                "[persistence] Failed to deserialize resource '{}': {}. Keeping the freshly initialized instance.",
                std::any::type_name::<T>(),
                error
            );
            false
        }
    }
}

/// Resolve the handle a captured entity identity may safely take.
///
/// Returns the captured `(id, generation)` pair unless the world reserved a
/// later generation for that id, in which case the reservation wins so the
/// restored handle can never alias a live one.
///
/// The reservation map is updated as each handle is installed, so a snapshot
/// that names the same id twice - which only a corrupted payload can do -
/// yields two distinct handles instead of one duplicated key.
pub(crate) fn resolve_explicit_entity(
    captured: SnapshotEntityId,
    reserved_generations: &mut HashMap<u64, u32>,
) -> Entity {
    let generation = reserved_generations
        .get(&captured.id)
        .copied()
        .map_or(captured.generation, |reserved| {
            captured.generation.max(reserved)
        });
    reserved_generations.insert(captured.id, generation.wrapping_add(1));
    Entity::from_parts(captured.id, generation)
}

/// Downcast and push a boxed component into the concrete VecStorage.
fn insert_boxed_component<T>(
    storage: &mut TraitTypeMap<dyn Component, VecFamily>,
    component: Box<dyn Component>,
) where
    T: Component + TraitAccessible<dyn Component> + 'static,
{
    // SAFETY: `raw` is produced by `Box::into_raw` above, so it is valid,
    // correctly aligned, and uniquely owned with no aliasing references
    // outstanding, and `Box::from_raw` takes ownership back exactly once.
    // The `*mut T` cast is sound because the concrete type of `component`
    // is guaranteed to match `T`: the caller resolves this function pointer
    // via the type-name lookup in `restore_from_snapshot`, so the fat
    // pointer's data and vtable are valid for `T`.  `*typed` is moved out
    // and pushed into storage, ending the box's ownership without a
    // double-free.
    let raw = Box::into_raw(component);
    let typed: Box<T> = unsafe { Box::from_raw(raw as *mut T) };
    storage.get_storage_mut::<T>().push(*typed);
}

// =============================================================================
// World — Additional Fields
// =============================================================================
//
// These fields are added to the `World` struct (see `world.rs`):
//
// ```ignore
// /// Per-component-type serialize fn for snapshotting.
// pub(crate) persist_serializers: HashMap<ComponentId, SerializeComponentFn>,
// /// Per-type-name deserialize fn for restoring.
// pub(crate) persist_deserializers: HashMap<String, DeserializeComponentFn>,
// /// Per-component-type insert fn for pushing Box<dyn Component> into storage.
// pub(crate) persist_inserters: HashMap<ComponentId, InsertComponentFn>,
// ```

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // External crates
    use serde::{Deserialize, Serialize};
    use trait_type_map::impl_trait_accessible;

    // Current crate
    use crate::resource::Resource;

    /// A persistable component used to exercise snapshot and restore.
    #[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
    struct SpatialPosition {
        horizontal: f32,
        vertical: f32,
    }
    impl Component for SpatialPosition {}

    /// A second persistable component so multi-archetype worlds are covered.
    #[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
    struct FrameCounter {
        count: u64,
    }
    impl Component for FrameCounter {}

    impl_trait_accessible!(dyn Component; SpatialPosition, FrameCounter);

    /// A persistable resource used to exercise resource capture and restore.
    #[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
    struct SimulationClock {
        elapsed_seconds: f64,
        frame_index: u64,
    }
    impl Resource for SimulationClock {}

    /// A resource with no serde support, recreated instead of captured.
    struct GpuHandle {
        _opaque: usize,
    }
    impl Resource for GpuHandle {}

    /// Build a world with both persistable component types registered.
    fn world_with_persistable_components() -> World {
        let mut world = World::new();
        world.register_persistable_component::<SpatialPosition>();
        world.register_persistable_component::<FrameCounter>();
        world
    }

    /// Read one entity's spatial position, if it still carries the component.
    fn read_position(world: &World, entity: Entity) -> Option<SpatialPosition> {
        world.get_component::<SpatialPosition>(entity).cloned()
    }

    /// A capture/restore round trip reinstates the exact captured handles.
    #[test]
    fn entity_identities_survive_a_snapshot_round_trip() {
        let mut world = world_with_persistable_components();
        let first = world
            .create_entity()
            .with(SpatialPosition {
                horizontal: 1.0,
                vertical: 2.0,
            })
            .build()
            .unwrap();
        let second = world
            .create_entity()
            .with(SpatialPosition {
                horizontal: 3.0,
                vertical: 4.0,
            })
            .with(FrameCounter { count: 7 })
            .build()
            .unwrap();

        let snapshot = world.snapshot_components();
        assert_eq!(snapshot.entity_ids.len(), snapshot.entries.len());

        // Restore into a *different* world, which is what an engine swap does.
        let mut replacement = world_with_persistable_components();
        replacement.restore_from_snapshot(&snapshot);

        assert_eq!(replacement.entity_count(), 2);
        assert!(replacement.is_entity_valid(first));
        assert!(replacement.is_entity_valid(second));
        assert_eq!(
            read_position(&replacement, second),
            Some(SpatialPosition {
                horizontal: 3.0,
                vertical: 4.0
            })
        );
    }

    /// Entities the replacement world created before restore do not force a
    /// generation bump, because restore destroys them first.
    #[test]
    fn identities_survive_a_replacement_world_that_seeded_entities_first() {
        let mut world = world_with_persistable_components();
        let captured_entity = world
            .create_entity()
            .with(FrameCounter { count: 42 })
            .build()
            .unwrap();
        let snapshot = world.snapshot_components();

        // Mimic `project_init` running in the replacement engine: it seeds its
        // own entities, which restore then destroys.
        let mut replacement = world_with_persistable_components();
        let _seeded = replacement
            .create_entity()
            .with(FrameCounter { count: 0 })
            .build()
            .unwrap();
        replacement.restore_from_snapshot(&snapshot);

        assert_eq!(replacement.entity_count(), 1);
        assert!(replacement.is_entity_valid(captured_entity));
        assert_eq!(
            replacement
                .get_component::<FrameCounter>(captured_entity)
                .map(|counter| counter.count),
            Some(42)
        );
    }

    /// A live entity at the requested id forces the next generation, so an
    /// explicit allocation can never alias a handle still in use.
    #[test]
    fn explicit_allocation_bumps_past_a_live_entity() {
        let mut world = world_with_persistable_components();
        let live = world
            .create_entity()
            .with(FrameCounter { count: 1 })
            .build()
            .unwrap();

        let reinstated = world.allocate_entity_at(live.id(), live.generation());
        assert_eq!(reinstated.id(), live.id());
        assert_eq!(reinstated.generation(), live.generation() + 1);
    }

    /// An explicit allocation moves the fresh-id counter past the id it took.
    #[test]
    fn explicit_allocation_reserves_the_id_for_good() {
        let mut world = World::new();
        let restored = world.allocate_entity_at(9, 4);
        assert_eq!((restored.id(), restored.generation()), (9, 4));

        let next = world.allocate_entity();
        assert!(
            next.id() > 9,
            "fresh ids must not collide with restored ones"
        );
    }

    /// A snapshot without identities still restores, using fresh handles.
    #[test]
    fn snapshots_without_identities_fall_back_to_fresh_handles() {
        let snapshot = ComponentSnapshot {
            entries: vec![vec![(
                std::any::type_name::<FrameCounter>().to_string(),
                serde_json::to_vec(&FrameCounter { count: 5 }).unwrap(),
            )]],
            entity_ids: Vec::new(),
        };

        let mut world = world_with_persistable_components();
        world.restore_from_snapshot(&snapshot);
        assert_eq!(world.entity_count(), 1);
    }

    /// A truncated identity list is discarded rather than mispaired.
    #[test]
    fn mismatched_identity_lists_are_discarded_on_restore() {
        let type_name = std::any::type_name::<FrameCounter>().to_string();
        let payload = serde_json::to_vec(&FrameCounter { count: 5 }).unwrap();
        let snapshot = ComponentSnapshot {
            entries: vec![
                vec![(type_name.clone(), payload.clone())],
                vec![(type_name, payload)],
            ],
            entity_ids: vec![SnapshotEntityId {
                id: 40,
                generation: 2,
            }],
        };

        let mut world = world_with_persistable_components();
        world.restore_from_snapshot(&snapshot);
        assert_eq!(world.entity_count(), 2);
        // The single captured identity must not have been applied to either
        // entity, because it could not be paired reliably.
        assert!(!world.is_entity_valid(Entity::from_parts(40, 2)));
    }

    /// A corrupted snapshot naming one id twice yields two distinct handles.
    #[test]
    fn duplicate_captured_identities_are_disambiguated() {
        let type_name = std::any::type_name::<FrameCounter>().to_string();
        let payload = serde_json::to_vec(&FrameCounter { count: 1 }).unwrap();
        let duplicate = SnapshotEntityId {
            id: 3,
            generation: 0,
        };
        let snapshot = ComponentSnapshot {
            entries: vec![
                vec![(type_name.clone(), payload.clone())],
                vec![(type_name, payload)],
            ],
            entity_ids: vec![duplicate, duplicate],
        };

        let mut world = world_with_persistable_components();
        world.restore_from_snapshot(&snapshot);
        assert_eq!(world.entity_count(), 2);
        assert!(world.is_entity_valid(Entity::from_parts(3, 0)));
        assert!(world.is_entity_valid(Entity::from_parts(3, 1)));
    }

    /// Registered resources are captured and restored; unregistered ones are not.
    #[test]
    fn persistable_resources_round_trip_and_others_are_left_alone() {
        let mut world = World::new();
        world.register_persistable_resource::<SimulationClock>();
        world.insert_resource(SimulationClock {
            elapsed_seconds: 12.5,
            frame_index: 750,
        });
        world.insert_resource(GpuHandle { _opaque: 1 });

        let captured = world.snapshot_resources();
        assert_eq!(captured.len(), 1, "only registered resources are captured");

        // Restore into a replacement world that re-created both resources the
        // way `project_init` would.
        let mut replacement = World::new();
        replacement.register_persistable_resource::<SimulationClock>();
        replacement.insert_resource(SimulationClock::default());
        replacement.insert_resource(GpuHandle { _opaque: 2 });

        let report = replacement.restore_resources(&captured);
        assert_eq!(report.restored_count, 1);
        assert!(report.skipped_type_names.is_empty());
        assert_eq!(
            replacement.get_resource::<SimulationClock>(),
            Some(&SimulationClock {
                elapsed_seconds: 12.5,
                frame_index: 750,
            })
        );
        // The non-serde resource keeps the instance the replacement created.
        assert!(replacement.has_resource::<GpuHandle>());
    }

    /// A resource type the project no longer registers is skipped, not fatal.
    #[test]
    fn resources_of_unregistered_types_are_skipped() {
        let mut world = World::new();
        let captured = vec![(
            String::from("project::RemovedResource"),
            b"{\"value\":1}".to_vec(),
        )];

        let report = world.restore_resources(&captured);
        assert_eq!(report.restored_count, 0);
        assert_eq!(
            report.skipped_type_names,
            vec![String::from("project::RemovedResource")]
        );
    }

    /// A registered resource whose payload no longer decodes is skipped and
    /// the freshly initialized instance is kept.
    #[test]
    fn undecodable_resource_payloads_keep_the_fresh_instance() {
        let mut world = World::new();
        world.register_persistable_resource::<SimulationClock>();
        world.insert_resource(SimulationClock {
            elapsed_seconds: 3.0,
            frame_index: 9,
        });

        let captured = vec![(
            std::any::type_name::<SimulationClock>().to_string(),
            b"not json at all".to_vec(),
        )];
        let report = world.restore_resources(&captured);

        assert_eq!(report.restored_count, 0);
        assert_eq!(report.skipped_type_names.len(), 1);
        assert_eq!(
            world.get_resource::<SimulationClock>(),
            Some(&SimulationClock {
                elapsed_seconds: 3.0,
                frame_index: 9,
            })
        );
    }

    /// Re-registering a resource replaces the previous entry instead of
    /// leaving a stale function pointer behind.
    #[test]
    fn resource_registration_is_idempotent_by_type_name() {
        let mut world = World::new();
        world.register_persistable_resource::<SimulationClock>();
        world.register_persistable_resource::<SimulationClock>();
        world.insert_resource(SimulationClock::default());

        assert_eq!(world.snapshot_resources().len(), 1);
    }

    /// The manifest reports one entry per registered persistable component.
    #[test]
    fn persist_manifest_covers_every_registered_component() {
        let world = world_with_persistable_components();
        let manifest = world.persist_type_manifest();

        assert_eq!(manifest.len(), 2);
        assert!(manifest
            .iter()
            .any(|entry| entry.type_name.ends_with("SpatialPosition")));
        assert!(manifest
            .iter()
            .any(|entry| entry.type_name.ends_with("FrameCounter")));
    }
}
