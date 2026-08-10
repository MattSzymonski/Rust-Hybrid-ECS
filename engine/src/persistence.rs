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
//! These functions are monomorphized in the game DLL (where the concrete
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
use std::collections::{HashMap, HashSet};

// External crates
use serde::{Serialize, de::DeserializeOwned};
use trait_type_map::{TraitAccessible, TraitTypeMap, VecFamily};

// Current crate
use crate::component::{Component, ComponentId};
use crate::entity::Entity;
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

// =============================================================================
// ComponentSnapshot
// =============================================================================

/// Captured component data for all entities at a point in time.
///
/// Used to preserve game state across hot-reloads.  Components are
/// matched by type **name** (a string like `"game::Position"`), not by
/// `TypeId`, so schema changes (added/removed fields) are handled by
/// serde's default-value / ignore-unknown behaviour.
#[derive(Debug, Default)]
pub struct ComponentSnapshot {
    /// Each entry represents one entity to recreate.
    /// Inner vec: list of `(component_type_name, json_bytes)`.
    pub entries: Vec<Vec<(String, Vec<u8>)>>,
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
#[derive(Debug, Default)]
pub struct SelectiveMigrationReport {
    /// Number of component types that were migrated.
    pub migrated_type_count: usize,
    /// Number of entities touched by selective migration.
    pub migrated_entity_count: usize,
    /// Component type names that could not be migrated selectively.
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
    /// so that bincode can round-trip its data.  When the struct shape changes
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
        // Standard component registration (bit, storage factory, copier).
        self.register_component::<T>();

        let component_id = ComponentId::of::<T>();
        let type_name = std::any::type_name::<T>().to_string();

        // Remove stale persist entries from previous registrations of
        // the same type name.  This handles the case where a component
        // struct is changed and then changed back — the compiler may
        // assign the same TypeId, but old entries from intermediate
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

        // Store the monomorphized serialize function.
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
}

// =============================================================================
// Per-Type Monomorphized Functions
// =============================================================================
//
// These generic functions are monomorphized once per concrete component type
// inside the game DLL.  They are stored as plain `fn` pointers in the engine's
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
    // Deserialize the snapshot data into a generic JSON Value.
    let snapshot_json: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| {
            eprintln!(
                "[persistence] Failed to parse JSON for '{}': {}",
                std::any::type_name::<T>(),
                error
            );
        })
        .ok()?;

    // Build the default instance and serialize it to JSON.
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

    // Merge: default fields fill in where the snapshot is missing.
    // Snapshot values override defaults where present.
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
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let default_value = T::default();
    let default_json = serde_json::to_value(default_value).unwrap_or(serde_json::Value::Null);
    let normalized_schema = normalize_schema_shape(&default_json);
    let normalized_schema_string = serde_json::to_string(&normalized_schema)
        .unwrap_or_else(|_| "<schema-serialization-failed>".to_string());

    let mut hasher = DefaultHasher::new();
    std::any::type_name::<T>().hash(&mut hasher);
    std::mem::size_of::<T>().hash(&mut hasher);
    normalized_schema_string.hash(&mut hasher);
    hasher.finish()
}

/// Downcast and push a boxed component into the concrete VecStorage.
fn insert_boxed_component<T>(
    storage: &mut TraitTypeMap<dyn Component, VecFamily>,
    component: Box<dyn Component>,
) where
    T: Component + TraitAccessible<dyn Component> + 'static,
{
    // SAFETY: This function is only called when the concrete type of
    // `component` matches `T`, as guaranteed by the type-name lookup
    // in `restore_from_snapshot`.  We transmute the fat pointer.
    let raw = Box::into_raw(component);
    let typed: Box<T> = unsafe { Box::from_raw(raw as *mut T) };
    storage.get_storage_mut::<T>().push(*typed);
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
    /// Panics if a persistable component's bincode serialization fails
    /// (should never happen for valid component types).
    pub fn snapshot_components(&self) -> ComponentSnapshot {
        let total_entities = self.entity_locations.len();
        let total_archetypes = self.archetypes.len();
        let persistable_type_count = self.persist_serializers.len();

        println!(
            "[persistence] Snapshotting {} entities across {} archetypes ({} persistable component types)...",
            total_entities, total_archetypes, persistable_type_count,
        );

        let mut entries: Vec<Vec<(String, Vec<u8>)>> = Vec::with_capacity(total_entities);
        let mut total_components_serialized: usize = 0;
        let mut skipped_non_persistable: usize = 0;

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

        ComponentSnapshot { entries }
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

        // Phase 1: Destroy all existing entities.
        let all_entity_ids: Vec<Entity> = self.entity_locations.keys().copied().collect();
        let destroyed_count = all_entity_ids.len();
        for entity in all_entity_ids {
            let _ = self.destroy_entity(entity);
        }
        println!(
            "[persistence]   Destroyed {} old entities (stale TypeIds)",
            destroyed_count,
        );

        // Phase 2: Recreate entities from snapshot data.
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

            let entity = self.allocate_entity();
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
        if skipped_type_removed > 0 {
            println!(
                "[persistence]   {} components skipped (type removed from game)",
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
    /// reload) and the new deserializer/inserter (registered by new game_init)
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
                Err(()) => {
                    println!("[persistence]   '{}' -> SKIP (migration failed)", type_name,);
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

    fn migrate_single_component_type(
        &mut self,
        type_name: &str,
        previous_metadata: &PersistTypeMetadata,
    ) -> Result<usize, ()> {
        let Some(new_component_id) = self.resolve_component_id_by_name(type_name) else {
            return Err(());
        };

        let Some(&deserialize_component) = self.persist_deserializers.get(type_name) else {
            return Err(());
        };

        let Some(&insert_component) = self.persist_inserters.get(&new_component_id) else {
            return Err(());
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

    fn migrate_component_column_in_place(
        &mut self,
        component_id: ComponentId,
        serialize_old_component: SerializeComponentFn,
        deserialize_new_component: DeserializeComponentFn,
        insert_new_component: InsertComponentFn,
    ) -> Result<usize, ()> {
        let archetype_ids: Vec<_> = self
            .archetypes
            .iter()
            .filter(|(_, archetype)| archetype.component_types.contains(&component_id))
            .map(|(archetype_id, _)| *archetype_id)
            .collect();

        let mut migrated_entity_count: usize = 0;

        for archetype_id in archetype_ids {
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

            let mut migrated_components: Vec<Box<dyn Component>> =
                Vec::with_capacity(serialized_components.len());

            for bytes in &serialized_components {
                let component = deserialize_new_component(bytes)
                    .or_else(|| deserialize_new_component(b"{}"))
                    .ok_or(())?;
                migrated_components.push(component);
            }

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
                return Err(());
            }

            let Some(storage_factory) = self.storage_factories.get(&component_id) else {
                return Err(());
            };
            let crate::archetype::StorageFactory::Native(storage_factory) = storage_factory else {
                return Err(());
            };
            storage_factory(&mut archetype.component_storages);

            for component in migrated_components {
                insert_new_component(&mut archetype.component_storages, component);
            }

            migrated_entity_count += serialized_components.len();
        }

        Ok(migrated_entity_count)
    }

    fn migrate_component_across_archetypes(
        &mut self,
        old_component_id: ComponentId,
        new_component_id: ComponentId,
        serialize_old_component: SerializeComponentFn,
        deserialize_new_component: DeserializeComponentFn,
        insert_new_component: InsertComponentFn,
    ) -> Result<usize, ()> {
        let source_archetype_ids: Vec<_> = self
            .archetypes
            .iter()
            .filter(|(_, archetype)| archetype.component_types.contains(&old_component_id))
            .map(|(archetype_id, _)| *archetype_id)
            .collect();

        let mut migrated_entity_count: usize = 0;

        for source_archetype_id in source_archetype_ids {
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
                    .ok_or(())?;
                migrated_components.push(component);
            }

            let Some(destination_archetype) = self.archetypes.get_mut(&destination_archetype_id)
            else {
                return Err(());
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
                        return Err(());
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

        self.archetype_generation = self.archetype_generation.wrapping_add(1);
        Ok(migrated_entity_count)
    }
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
