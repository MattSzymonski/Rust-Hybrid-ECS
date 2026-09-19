//! Registration bookkeeping: manifests, sequence markers, and forget paths.
//!
//! # Responsibilities
//!
//! - Answers what this generation registers: the persistable type manifest
//!   and the sequence markers the host captures before a module's `init`.
//! - Enumerates what one `init` registered afterwards, by name and by id, so
//!   a type a reloaded module dropped is detected instead of orphaned.
//! - Forgets registrations whose owner is gone: drops their columns and
//!   releases their bits, keeping name-keyed entries only while a surviving
//!   registration still claims the name.
//!
//! # Design
//!
//! The registry accumulates one entry per generation, each with its own
//! `TypeId` for the same name, so the forget paths sweep by name and retire
//! every id they find. Retiring is careful to release a bit only when no
//! archetype still maps it: the mask is the archetype id, so releasing a bit
//! with live rows behind it would alias two component sets onto one column.

use super::*;

// =============================================================================
// World — Registration Bookkeeping
// =============================================================================

impl World {
    /// Look up the single [`ComponentId`] a component type name resolves to.
    ///
    /// After multiple hot-reloads the component registry can hold one entry
    /// per reload, each with a different `TypeId` but the same type name.
    /// What reduces those to one candidate is **eviction**: registering a new
    /// generation purges every same-name entry from `persist_inserters`
    /// (see [`Self::register_persistable_component`]), so the
    /// `persist_inserters` filter below leaves exactly one.
    ///
    /// The bit index is deliberately *not* used as a tiebreak. It is not a
    /// recency ordering: `ComponentRegistry::allocate_bit` hands out bits
    /// reclaimed by `remove` before advancing `next_bit`, so a later
    /// registration can receive a lower bit than an earlier one. Since
    /// eviction already guarantees uniqueness, more than one surviving
    /// candidate is a bug rather than something to break a tie on, and it is
    /// reported as [`WorldError::ComponentNameAmbiguous`].
    ///
    /// [`persist_registration_sequence`](Self::persist_registration_sequence)
    /// is the true chronological ordering, if a recency tiebreak is ever
    /// genuinely wanted.
    pub(super) fn resolve_component_id_by_name(
        &self,
        type_name: &str,
    ) -> Result<Option<ComponentId>, WorldError> {
        let mut candidates = self
            .component_registry
            .registered_components()
            .filter(|(_, _, name)| *name == type_name)
            .filter(|(id, _, _)| self.persist_inserters.contains_key(id))
            .map(|(id, _, _)| id);

        let Some(first) = candidates.next() else {
            return Ok(None);
        };
        // Any second candidate means eviction did not collapse the set, so
        // picking either one would silently bind half the rows to the wrong
        // column.
        let extra = candidates.count();
        if extra > 0 {
            return Err(WorldError::ComponentNameAmbiguous {
                type_name: type_name.to_string(),
                count: extra + 1,
            });
        }
        Ok(Some(first))
    }

    /// [`Self::resolve_component_id_by_name`] for callers that cannot return
    /// an error, degrading an ambiguous name to "unresolved".
    ///
    /// Every such caller already has a safe answer for an unresolved name -
    /// skip the row, omit the manifest entry, drop nothing - so reporting the
    /// ambiguity and taking that path is strictly better than the previous
    /// `max_by_key(bit)` tiebreak, which silently picked one of the candidates
    /// and could bind rows to the wrong column.
    pub(super) fn resolve_component_id_by_name_logged(
        &self,
        type_name: &str,
    ) -> Option<ComponentId> {
        match self.resolve_component_id_by_name(type_name) {
            Ok(component_id) => component_id,
            Err(error) => {
                error!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    type_name = %type_name,
                    error = %error,
                    "component type name is ambiguous; treating it as unresolved"
                );
                None
            }
        }
    }

    /// Return current persistable component manifest.
    pub fn persist_type_manifest(&self) -> Vec<PersistTypeManifestEntry> {
        let mut entries: Vec<PersistTypeManifestEntry> = self
            .persist_schema_hashes
            .iter()
            .filter_map(|(type_name, schema_hash)| {
                self.resolve_component_id_by_name_logged(type_name)
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

    /// Sequence marker to capture before a module's `init` so the exact set of
    /// persistable types that `init` registers can be enumerated afterwards.
    pub fn persist_registration_sequence(&self) -> u64 {
        self.persist_registration_sequence
    }

    /// Names of persistable components registered after `sequence`.
    ///
    /// Used by the host to compare what a module's new generation registered
    /// against what the previous generation registered, so a component type
    /// that was accidentally dropped from the module is detected instead of
    /// silently orphaned.
    ///
    /// The comparison is `>=`: a registration reads the current sequence value
    /// and then increments it, so a registration that happened immediately
    /// after the capture carries exactly the captured value.
    pub fn persist_type_names_registered_since(&self, sequence: u64) -> Vec<String> {
        self.persist_registration_log
            .iter()
            .filter(|(_, registration_sequence)| *registration_sequence >= sequence)
            .map(|(type_name, _)| type_name.clone())
            .collect()
    }

    /// Sequence marker to capture before a module's `init` so the exact set of
    /// component types (plain or persistable) that `init` registers can be
    /// enumerated afterwards.
    pub fn component_registration_sequence(&self) -> u64 {
        self.component_registration_sequence
    }

    /// Names of all component types registered after `sequence`.
    ///
    /// Unlike [`Self::persist_type_names_registered_since`], this covers plain
    /// components too, so the host can tell a type a reloaded module dropped
    /// entirely apart from one merely downgraded from persistable to plain
    /// (whose data is still live).
    pub fn registered_component_names_since(&self, sequence: u64) -> Vec<String> {
        self.component_registration_log
            .iter()
            .filter(|(_, _, registration_sequence)| *registration_sequence >= sequence)
            .map(|(type_name, _, _)| type_name.clone())
            .collect()
    }

    /// Ids of all component types registered after `sequence`.
    ///
    /// The id-level twin of [`Self::registered_component_names_since`]: a
    /// rebuilt image gets a fresh `TypeId` for every name it declares, so a
    /// failed generation's entries can be told apart from a rollback
    /// generation's re-registration of the same names only by id.
    pub fn registered_component_ids_since(&self, sequence: u64) -> Vec<ComponentId> {
        self.component_registration_log
            .iter()
            .filter(|(_, _, registration_sequence)| *registration_sequence >= sequence)
            .map(|(_, component_id, _)| *component_id)
            .collect()
    }

    /// Drop every column belonging to component types that no module or
    /// project generation registers anymore.
    ///
    /// Called by the host right after a reloaded module/project stopped
    /// registering a persistable type entirely (it is not even registered as a
    /// plain component, so its data is truly orphaned). Removing the component
    /// from every entity frees its columns now, while the generation that last
    /// registered the type is still mapped, so the drop can never call into an
    /// evicted DLL. Returns the number of entities whose data was removed.
    ///
    /// Every native id sharing the name loses its rows, not just the one a
    /// name resolver returns: an ambiguous name resolves to nothing at all, and
    /// a superseded generation's rows would otherwise outlive the registration
    /// purge that `forget_component_type` then performs for all of them.
    pub fn drop_forgotten_components(&mut self, type_names: &[String]) -> usize {
        let mut dropped_entities = 0;
        for type_name in type_names {
            // Native columns only; type-erased foreign-language columns are
            // not part of the native forgotten-type path. Shared components
            // are native, so they are covered here. A name none of whose ids
            // is native is left alone entirely, including its name-keyed
            // entries: it belongs to a live peer, not to a forgotten type.
            let native_ids: Vec<ComponentId> = self
                .component_registry
                .registered_components()
                .filter(|(_, _, name)| *name == type_name)
                .map(|(component_id, _, _)| component_id)
                .filter(|component_id| component_id.is_native_storage())
                .collect();
            if native_ids.is_empty() {
                continue;
            }

            for component_id in native_ids {
                // Collect the entities carrying this component up front so the
                // mutable borrows during removal never overlap the iteration.
                let entities: Vec<Entity> = self
                    .entity_locations
                    .iter()
                    .filter(|(_, location)| {
                        self.archetypes
                            .get(&location.archetype_id)
                            .is_some_and(|archetype| {
                                archetype.component_types.contains(&component_id)
                            })
                    })
                    .map(|(entity, _)| *entity)
                    .collect();

                for entity in entities {
                    if self.remove_component_by_id(entity, component_id).is_ok() {
                        dropped_entities += 1;
                    }
                }
            }

            self.forget_component_type(type_name);
        }
        dropped_entities
    }

    /// Drop every column belonging to the given component ids and forget their
    /// registrations.
    ///
    /// The id-keyed counterpart of [`Self::drop_forgotten_components`], used by
    /// the host when a failed generation's registrations have to go: the ids
    /// are the only handle that separates them from a rollback generation's
    /// re-registration of the same names. Rows are removed through
    /// `remove_component_by_id`, so a type the failed generation's `init`
    /// managed to give rows to does not leak them into an image that is about
    /// to be retired. A name-keyed entry (deserializer, schema hash) goes only
    /// when no surviving registration still claims the name.
    ///
    /// The sweep itself lives in [`Self::retire_component_storage`], which is
    /// also its public face: the two differ in caller, not in what they do.
    pub fn drop_forgotten_component_ids(&mut self, component_ids: &[ComponentId]) -> usize {
        self.retire_component_storage(component_ids)
    }

    /// Retire the storage of the given component ids.
    ///
    /// The general form of the id-keyed sweep: every entity carrying an id
    /// loses that one component through `remove_component_by_id`, the
    /// registration is retired (its bit released when no archetype with live
    /// rows still references it, the rule [`Self::retire_registration`]
    /// documents), and a name-keyed entry (deserializer, schema hash) goes only
    /// when no surviving registration still claims the name.
    ///
    /// Covers the descriptor lane, unlike the name-based
    /// [`Self::drop_forgotten_components`]: a managed component has no Rust
    /// type to name it by, so the host reaches it through the id its binding
    /// held. That is also what the host uses when a reloaded manifest stopped
    /// naming a managed component.
    ///
    /// Returns how many entities lost data.
    pub fn retire_component_storage(&mut self, component_ids: &[ComponentId]) -> usize {
        let mut dropped_entities = 0;
        for &component_id in component_ids {
            let type_name = self
                .component_registry
                .registered_components()
                .find(|(id, _, _)| *id == component_id)
                .map(|(_, _, name)| name.to_string());

            // Collect the entities carrying this component up front so the
            // mutable borrows during removal never overlap the iteration.
            let entities: Vec<Entity> = self
                .entity_locations
                .iter()
                .filter(|(_, location)| {
                    self.archetypes
                        .get(&location.archetype_id)
                        .is_some_and(|archetype| archetype.component_types.contains(&component_id))
                })
                .map(|(entity, _)| *entity)
                .collect();

            for entity in entities {
                if self.remove_component_by_id(entity, component_id).is_ok() {
                    dropped_entities += 1;
                }
            }

            self.retire_registration(component_id);
            if let Some(name) = type_name {
                let still_claimed = self
                    .component_registry
                    .registered_components()
                    .any(|(_, _, other)| other == name.as_str());
                if !still_claimed {
                    self.persist_deserializers.remove(&name);
                    self.persist_schema_hashes.remove(&name);
                }
            }
        }
        dropped_entities
    }

    /// Retire one registration, releasing its bit only when no archetype still
    /// maps a mask carrying it.
    ///
    /// The mask *is* the archetype id, so a bit that returns to the pool while
    /// an archetype references it aliases two component sets onto one id: the
    /// next registration that reuses the bit would reach the stale archetype
    /// through `get_or_create_archetype` and be served its columns. The sweeps
    /// that precede both callers rehome every entity holding the type, so the
    /// archetypes carrying the bit are empty and are dropped here. A non-empty
    /// one means rows still reference the bit, and then the registration is
    /// removed *without* releasing the bit - a leaked bit is recoverable, an
    /// aliased archetype is not.
    pub(super) fn retire_registration(&mut self, component_id: ComponentId) {
        let bit = self.component_registry.get_bit(&component_id);
        let rows_still_reference_bit = bit.is_some_and(|bit| {
            self.archetypes.values().any(|archetype| {
                archetype.component_mask.has_bit(bit) && !archetype.entities.is_empty()
            })
        });
        debug_assert!(
            !rows_still_reference_bit,
            "an archetype with live rows references the component bit being released"
        );
        if !rows_still_reference_bit {
            if let Some(bit) = bit {
                self.archetypes
                    .retain(|_, archetype| !archetype.component_mask.has_bit(bit));
            }
            self.component_registry.remove(&component_id);
        }
        self.storage_factories.remove(&component_id);
        self.retired_native_storage_ops.remove(&component_id);
        self.persist_serializers.remove(&component_id);
        self.persist_inserters.remove(&component_id);
    }

    /// Retire a registration for callers outside this module.
    ///
    /// [`Self::retire_registration`] is module-private because the sweeps that
    /// call it must run first; `World`'s descriptor remap performs that sweep
    /// itself (it empties the source before calling), and lives outside this
    /// module, so it needs the door.
    pub(crate) fn retire_component_registration(&mut self, component_id: ComponentId) {
        self.retire_registration(component_id);
    }

    /// Remove every registration artifact for one forgotten component type so
    /// it stops appearing in the persistable manifest and registry.
    pub(super) fn forget_component_type(&mut self, type_name: &str) {
        // The registry accumulates one entry per generation (each with a
        // distinct `TypeId`); purge every entry that shares this type name.
        let stale_ids: Vec<ComponentId> = self
            .component_registry
            .registered_components()
            .filter(|(_, _, name)| *name == type_name)
            .map(|(id, _, _)| id)
            .collect();
        for stale_id in &stale_ids {
            self.retire_registration(*stale_id);
        }
        self.persist_deserializers.remove(type_name);
        self.persist_schema_hashes.remove(type_name);
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

    /// Snapshot every live entity id.
    ///
    /// Migration runs after the incoming generation's `init`, so archetypes
    /// may already hold entities whose components were written with the
    /// current schema. This set lets the migration tell those apart from the
    /// entities that need converting - see
    /// [`Self::migrate_changed_persistable_components`].
    pub fn capture_live_entities(&self) -> HashSet<Entity> {
        let mut entities: HashSet<Entity> = HashSet::new();
        for archetype in self.archetypes.values() {
            entities.extend(archetype.entities.iter().copied());
        }
        entities
    }
}

// =============================================================================
// Function Pointer Type Aliases
// =============================================================================

/// Serializes the component at `index` in the given storage map into a
/// JSON-encoded byte vector.
pub(crate) type SerializeComponentFn = fn(storage: &ComponentColumns, index: usize) -> Vec<u8>;

/// Deserializes JSON bytes back into a heap-allocated component.
///
/// Returns `None` if the schema changed incompatibly (e.g. a field type
/// changed from `f32` to `String`).  Simple additions/removals are handled
/// by serde's `default` / `ignore` behaviour.
pub(crate) type DeserializeComponentFn = fn(bytes: &[u8]) -> Option<Box<dyn Component>>;

/// Downcasts a `Box<dyn Component>` to its concrete type and pushes it
/// into the appropriate `VecStorage<T>` inside the storage map.
pub(crate) type InsertComponentFn =
    fn(storage: &mut ComponentColumns, component: Box<dyn Component>);

/// Serializes one registered resource out of the world into JSON bytes.
///
/// `None` when the world holds no value for it, which is different from an
/// empty value: a resource that was never inserted has nothing to record.
/// Takes the whole world rather than a value because a resource is found by
/// its id, and the id is what monomorphizing this for `T` supplies.
pub type SerializeResourceFn = fn(world: &World) -> Option<Vec<u8>>;

/// Rebuilds one resource from snapshot bytes and inserts it into the world.
///
/// Returns whether the payload could be read. A value that no longer parses
/// under the current shape is reported and skipped rather than inserted
/// half-formed, matching how an unreadable component row is handled.
pub type RestoreResourceFn = fn(world: &mut World, bytes: &[u8]) -> bool;

// =============================================================================
// Per-Type Monomorphized Functions
// =============================================================================

//
// These generic functions are monomorphized once per concrete component type
// inside the project DLL.  They are stored as plain `fn` pointers in the engine's
// HashMaps, so replacing them on hot-reload simply overwrites the pointer —
// no destructors, no vtable calls, no DLL-unload issues.

/// Deep-merge two JSON values: `base` provides defaults, `override_json`
/// provides the actual data.  Fields present in `override_json` take
/// precedence; fields only in `base` are kept as defaults.
pub(super) fn merge_json(
    mut base: serde_json::Value,
    override_json: serde_json::Value,
) -> serde_json::Value {
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
pub(super) fn normalize_schema_shape(value: &serde_json::Value) -> serde_json::Value {
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
///
/// `fields` is the declared field layout when the type was registered through
/// the derive, and empty for hand-registered types. Including it matters:
/// normalized default values describe a `Vec<f32>` and a `Vec<String>` alike
/// as an empty array, so without the tags a reload could take the
/// unchanged-schema fast path and then interpret one as the other.
pub(super) fn calculate_schema_hash<T>(
    fields: &[crate::component_registry::ComponentFieldDescriptor],
) -> u64
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

    // Step 3: Fold in the declared field layout. Tags carry the container
    // kinds (`vec:f32`, `string`), offsets distinguish a reordered or padded
    // shape, and the element count separates a fixed array from a resized one.
    // A layout-less registration contributes nothing here, keeping its hash
    // exactly what earlier builds produced.
    for field in fields {
        field.name.hash(&mut hasher);
        field.type_tag.hash(&mut hasher);
        field.offset.hash(&mut hasher);
        field.size.hash(&mut hasher);
        field.align.hash(&mut hasher);
        field.element_count.hash(&mut hasher);
    }
    hasher.finish()
}
