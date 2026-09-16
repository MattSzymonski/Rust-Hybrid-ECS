//! Resource-side persistence: the snapshot shape and the per-type manifest.
//!
//! # Responsibilities
//!
//! - Defines [`ResourceSnapshot`], the flat `(name, payload)` capture of
//!   resource values, and [`PersistResourceManifestEntry`], the registration
//!   view the reload transaction compares across the swap.
//! - Owns the resource restore path: the registered lane reads a payload back
//!   through the glue that wrote it, the foreign lane keeps opaque bytes.
//!
//! # Design
//!
//! A resource is a singleton, so unlike components there is no entity to group
//! by and the snapshot is a flat, name-sorted list. Sorting by name makes a
//! snapshot reproducible, which matters because the reload transaction
//! compares snapshots across two generations of the same project.

use super::*;

// =============================================================================
// ResourceSnapshot
// =============================================================================

/// Captured resource values at a point in time.
///
/// The resource twin of [`ComponentSnapshot`], and keyed the same way and for
/// the same reason: an entry names its resource by the string both generations
/// write down, never by an id, because an id is a `TypeId` in one process and a
/// name hash in another while the name is what a save file can carry.
///
/// Flat rather than nested: a resource is a singleton, so there is no entity to
/// group by.
///
/// # Examples
///
/// ```
/// use pill_engine::ResourceSnapshot;
///
/// let snapshot = ResourceSnapshot {
///     entries: vec![("project::Score".to_string(), b"{\"points\":7}".to_vec())],
/// };
/// assert_eq!(snapshot.resource_count(), 1);
/// ```
#[derive(Debug, Default, Clone)]
pub struct ResourceSnapshot {
    /// One `(registered name, JSON payload)` pair per resource that held a
    /// value, sorted by name so a snapshot is reproducible.
    pub entries: Vec<(String, Vec<u8>)>,
}

impl ResourceSnapshot {
    /// Number of resource values stored.
    #[must_use]
    pub fn resource_count(&self) -> usize {
        self.entries.len()
    }

    /// The payload recorded for one resource name, if the snapshot has it.
    #[must_use]
    pub fn payload(&self, type_name: &str) -> Option<&[u8]> {
        self.entries
            .iter()
            .find(|(name, _)| name == type_name)
            .map(|(_, payload)| payload.as_slice())
    }
}

/// One persistable resource registration, as the reload transaction sees it.
///
/// Captured before the swap by [`World::persist_resource_manifest`] so the pass
/// afterwards can compare schema hashes and read a reshaped value through the
/// glue that wrote it. The component analogue is [`PersistTypeMetadata`].
#[derive(Clone)]
pub struct PersistResourceManifestEntry {
    /// The id this resource was registered under in that generation.
    pub resource_id: ResourceId,
    /// The name the resource is recorded under, stable across generations.
    pub type_name: String,
    /// Structural hash of the shape this generation declared.
    pub schema_hash: u64,
    /// Serializer belonging to that generation, able to read its stored value.
    pub serializer: SerializeResourceFn,
}

// =============================================================================
// World — Persistable Resource Registration
// =============================================================================
//
// The resource half of the same contract. A resource is a singleton, so there
// is no archetype to walk and no per-entity fan-out: a snapshot is one payload
// per registered resource, keyed by the name both generations agree on.
//
// Two things make this worth having beyond "save files should include the
// resources". First, a world written to disk without its resources loses the
// project's whole global state - the score, the level index, the settings -
// while looking like a complete save. Second, `rehome_resources` refreshes a
// live resource's function table across a reload but never touches its bytes,
// so a resource whose shape changed is reinterpreted under the new type unless
// something migrates it. `migrate_changed_persistable_resources` is that
// something, and it is the resource twin of the component migration the reload
// transaction already runs.

impl World {
    /// Register a resource that survives snapshot, restore and reload.
    ///
    /// The resource counterpart of [`Self::register_persistable_component`],
    /// spelled as a registration call rather than a derive attribute because
    /// resources have no derive: a resource is declared by `impl Resource` and
    /// registered by hand, so opting into persistence is one more call at the
    /// same site.
    ///
    /// Registration is what opts a resource in. A resource registered with
    /// [`Self::register_resource`] alone keeps behaving exactly as before and
    /// simply never appears in a snapshot - the same relationship
    /// `register_component` has with `register_persistable_component`.
    ///
    /// `T` must implement `Serialize + DeserializeOwned + Default` for the same
    /// reason a persistable component must: JSON is what crosses the gap
    /// between two shapes of the type, and `Default` supplies the fields a
    /// snapshot written before those fields existed cannot carry.
    pub fn register_persistable_resource<T>(&mut self)
    where
        T: Resource + Serialize + DeserializeOwned + Default,
    {
        // A shared resource is known everywhere by its declared name; a
        // private one has only its Rust path. Both builds of one type derive
        // the same string, which is what makes a snapshot portable across a
        // reload and across a process.
        let type_name = T::shared_name()
            .map(str::to_owned)
            .unwrap_or_else(|| std::any::type_name::<T>().to_owned());
        self.register_persistable_resource_as::<T>(&type_name);
    }

    /// Register a persistable resource under an explicit persistence name.
    ///
    /// The name is the only thing a snapshot and a reload have to match on, so
    /// pinning it decouples save compatibility from the Rust path: moving a
    /// resource between modules, or renaming the crate it lives in, then costs
    /// nothing. It is also what a foreign declaration needs, because a managed
    /// resource's name is not a Rust type path.
    ///
    /// Two different Rust types may share one persistence name, and that is the
    /// reload case rather than a mistake: the arriving build of a type is a
    /// different type as far as this process is concerned. The later
    /// registration replaces the earlier one's entries, and the value the
    /// earlier one owns is carried across by
    /// [`Self::migrate_changed_persistable_resources`].
    pub fn register_persistable_resource_as<T>(&mut self, type_name: &str)
    where
        T: Resource + Serialize + DeserializeOwned + Default,
    {
        // The ordinary registration runs first and owns the refusal: a shared
        // name claimed by another type must stop the persist entries from
        // being written too, or a refused generation would still be able to
        // read and overwrite the live value through its own glue.
        let id = ResourceId::of::<T>();
        if !self.register_resource_claiming::<T>() {
            return;
        }
        let type_name = type_name.to_owned();

        // A previous generation's entry for this name is replaced rather than
        // joined: its serializer points into an image that is being retired,
        // and after a rebuild the same type can land on a different id. Left
        // in place, the stale entry would make a snapshot read through code
        // that is about to be unmapped and emit the name twice.
        let stale_ids: Vec<ResourceId> = self
            .persist_resource_names
            .iter()
            .filter(|(existing_id, existing_name)| {
                **existing_name == type_name && **existing_id != id
            })
            .map(|(existing_id, _)| *existing_id)
            .collect();
        for stale_id in stale_ids {
            self.persist_resource_names.remove(&stale_id);
            self.persist_resource_serializers.remove(&stale_id);
        }

        self.persist_resource_serializers
            .insert(id, serialize_resource::<T> as SerializeResourceFn);
        self.persist_resource_restorers.insert(
            type_name.clone(),
            restore_resource::<T> as RestoreResourceFn,
        );
        self.persist_resource_schema_hashes
            .insert(type_name.clone(), calculate_resource_schema_hash::<T>());
        self.persist_resource_names.insert(id, type_name);
    }

    /// Record the persistence name of a resource declared by another language.
    ///
    /// A foreign resource has no Rust type, so nothing can monomorphize a
    /// serializer for it - but it needs none: its payload is blittable bytes
    /// and the world already knows their size, so the bytes *are* the value.
    /// Recording the name is all that is needed to make it snapshot-visible,
    /// which is why this takes a name rather than a type parameter.
    ///
    /// Returns whether the id names a registered foreign resource; a caller
    /// that has just registered one can ignore the result.
    pub fn register_persistable_foreign_resource(&mut self, name: &str) -> bool {
        let id = ResourceId::Shared(crate::component::shared_component_identity(name));
        if !self
            .resource_factories
            .get(&id)
            .is_some_and(|ops| ops.foreign)
        {
            return false;
        }
        self.persist_resource_names.insert(id, name.to_owned());
        true
    }

    /// Whether the resource `T` is registered for persistence.
    #[must_use]
    pub fn is_resource_persistable<T: Resource>(&self) -> bool {
        self.persist_resource_names
            .contains_key(&ResourceId::of::<T>())
    }

    /// Capture the value of every persistable resource.
    ///
    /// Resources with no value stored yet are absent rather than written as
    /// null: a snapshot records what the world held, and "never inserted" is
    /// not a value. Restoring such a snapshot leaves the resource to whatever
    /// the loading generation inserts itself.
    #[must_use]
    pub fn snapshot_resources(&self) -> ResourceSnapshot {
        let mut entries = Vec::new();
        for (id, resource) in &self.resources {
            let Some(type_name) = self.persist_resource_names.get(id) else {
                continue;
            };
            // A foreign payload is captured byte for byte. There is no Rust
            // type to ask and nothing to interpret, so the bytes go in as a
            // JSON array - the same "preserve what you cannot read" rule the
            // descriptor component codec applies to an opaque field.
            if resource.is_foreign() {
                entries.push((type_name.clone(), serialize_foreign_resource(resource)));
                continue;
            }
            let Some(serialize) = self.persist_resource_serializers.get(id) else {
                continue;
            };
            if let Some(bytes) = serialize(self) {
                entries.push((type_name.clone(), bytes));
            }
        }
        // Sorted so a snapshot is reproducible. Hash-map order is not, and a
        // save file that differs between two identical runs cannot be diffed,
        // compared, or used as a test fixture.
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            "[persistence] Snapshotting {} resource(s)",
            entries.len(),
        );
        ResourceSnapshot { entries }
    }

    /// Restore resource values captured by [`Self::snapshot_resources`].
    ///
    /// A resource the loading generation does not declare is skipped, exactly
    /// as a component type the project stopped declaring is: a snapshot is a
    /// record of what was, not an instruction to recreate it.
    pub fn restore_resources(&mut self, snapshot: &ResourceSnapshot) {
        let mut restored = 0usize;
        let mut skipped = 0usize;
        for (type_name, payload) in &snapshot.entries {
            let accepted = match self.persist_resource_restorers.get(type_name).copied() {
                Some(restore) => restore(self, payload),
                // No Rust restorer means either a foreign resource, whose
                // bytes go back verbatim, or a name this generation dropped.
                None => self.restore_foreign_resource(type_name, payload),
            };
            if accepted {
                restored += 1;
            } else {
                skipped += 1;
            }
        }
        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            "[persistence] Restored {} resource(s), {} skipped",
            restored,
            skipped,
        );
    }

    /// Write a foreign resource's bytes back, when this generation declares it.
    ///
    /// Returns whether the payload was accepted. A size that disagrees with the
    /// current declaration is refused rather than padded: foreign bytes carry
    /// no field names, so a shorter or longer payload cannot be reconciled with
    /// the live layout without a migration plan, which only the declaring
    /// language can supply.
    pub(super) fn restore_foreign_resource(&mut self, type_name: &str, payload: &[u8]) -> bool {
        let id = ResourceId::Shared(crate::component::shared_component_identity(type_name));
        let Some(ops) = self.resource_factories.get(&id).copied() else {
            return false;
        };
        if !ops.foreign {
            return false;
        }
        let Some(bytes) = decode_foreign_resource_payload(payload, ops.size) else {
            warn!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                resource = %type_name,
                expected_size = ops.size,
                "[persistence] Foreign resource payload does not match the declared size; skipped"
            );
            return false;
        };
        self.insert_foreign_resource_bytes(id, &bytes).is_ok()
    }

    /// The persistable resources this generation currently declares.
    ///
    /// Captured before a reload's swap so the pass afterwards can compare
    /// schema hashes and migrate only the resources whose shape changed. The
    /// component twin is [`Self::persist_type_manifest`], and the two are used
    /// at the same two points in the reload transaction.
    #[must_use]
    pub fn persist_resource_manifest(&self) -> Vec<PersistResourceManifestEntry> {
        let mut entries: Vec<PersistResourceManifestEntry> = self
            .persist_resource_names
            .iter()
            .filter_map(|(id, type_name)| {
                Some(PersistResourceManifestEntry {
                    resource_id: *id,
                    type_name: type_name.clone(),
                    schema_hash: *self.persist_resource_schema_hashes.get(type_name)?,
                    serializer: *self.persist_resource_serializers.get(id)?,
                })
            })
            .collect();
        entries.sort_by(|left, right| left.type_name.cmp(&right.type_name));
        entries
    }

    /// Migrate every persistable resource whose schema changed across a reload.
    ///
    /// **Call this while the retiring generation's image is still mapped.** The
    /// serializer in `previous_manifest` was monomorphized for the *old* shape
    /// and lives in the outgoing artifact; it is the only code that can read
    /// the stored value correctly, which is why the capture has to happen
    /// before the swap and the migration before the graveyard eviction.
    ///
    /// The value is read through the old glue, carried across as JSON, and
    /// re-inserted through the new type's restorer, which merges it over the
    /// new `Default` - so a field added since the previous build arrives
    /// defaulted and a field removed is dropped, matching the component path
    /// exactly. Resources whose hash is unchanged are left untouched, keeping
    /// their allocation and their change ticks.
    ///
    /// Returns the names that were migrated, sorted.
    pub fn migrate_changed_persistable_resources(
        &mut self,
        previous_manifest: &[PersistResourceManifestEntry],
    ) -> Vec<String> {
        // Step 1: Read every changed resource through the outgoing glue before
        // anything is written back, so one failed restore cannot leave the
        // world half-migrated with the old image already consulted.
        let mut carried: Vec<(String, Vec<u8>)> = Vec::new();
        for entry in previous_manifest {
            let current_hash = self.persist_resource_schema_hashes.get(&entry.type_name);
            // Absent from this generation means the resource was dropped, not
            // reshaped; `drop_resources` owns that case. Equal hashes mean the
            // stored bytes are still valid under the new type.
            let Some(&current_hash) = current_hash else {
                continue;
            };
            if current_hash == entry.schema_hash {
                continue;
            }
            if let Some(payload) = (entry.serializer)(self) {
                carried.push((entry.type_name.clone(), payload));
            } else {
                warn!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    resource = %entry.type_name,
                    "[persistence] Reshaped resource held no readable value; leaving it to the new default"
                );
            }
        }

        // Step 2: Write each carried value back through the arriving type.
        let mut migrated = Vec::new();
        for (type_name, payload) in carried {
            let Some(restore) = self.persist_resource_restorers.get(&type_name).copied() else {
                continue;
            };
            if restore(self, &payload) {
                migrated.push(type_name);
            }
        }
        migrated.sort();
        if !migrated.is_empty() {
            debug!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                migrated_resources = ?migrated,
                "[persistence] Migrated reshaped resources"
            );
        }
        migrated
    }
}

// =============================================================================
// Per-Type Monomorphized Functions
// =============================================================================

/// Compute the schema hash for one persistable resource type.
///
/// Mirrors [`calculate_schema_hash`] minus the field-layout fold, which has no
/// resource counterpart: a Rust resource is not a column and declares no
/// blittable field table. A shared resource that *does* declare one publishes
/// it as [`Resource::shared_schema_hash`], which the claim check compares
/// separately - that hash guards cross-artifact agreement about a layout, while
/// this one guards migration across a rebuild, so they are deliberately not the
/// same number.
pub(super) fn calculate_resource_schema_hash<T>() -> u64
where
    T: Resource + Serialize + Default,
{
    // Normalized to kind markers so changing a default *value* does not read
    // as a schema change and force a pointless migration.
    let default_json = serde_json::to_value(T::default()).unwrap_or(serde_json::Value::Null);
    let normalized_schema = normalize_schema_shape(&default_json);
    let normalized_schema_string = serde_json::to_string(&normalized_schema)
        .unwrap_or_else(|_| "<schema-serialization-failed>".to_string());

    let mut hasher = DefaultHasher::new();
    std::any::type_name::<T>().hash(&mut hasher);
    std::mem::size_of::<T>().hash(&mut hasher);
    normalized_schema_string.hash(&mut hasher);
    hasher.finish()
}

/// Serialize the live value of resource `T` as JSON.
pub(super) fn serialize_resource<T>(world: &World) -> Option<Vec<u8>>
where
    T: Resource + Serialize,
{
    serde_json::to_vec(world.get_resource::<T>()?)
        .map_err(|error| {
            warn!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                "[persistence] Failed to serialize resource '{}': {}",
                std::any::type_name::<T>(),
                error
            );
        })
        .ok()
}

/// Rebuild resource `T` from JSON and insert it into the world.
///
/// The snapshot payload is merged over a default instance, so a field added
/// since the snapshot arrives defaulted and a field removed since is ignored -
/// the same name-matched reconciliation [`deserialize_component`] performs, and
/// the reason a reshaped resource keeps the parts of its value that still mean
/// something.
pub(super) fn restore_resource<T>(world: &mut World, bytes: &[u8]) -> bool
where
    T: Resource + Serialize + DeserializeOwned + Default,
{
    let snapshot_json: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(value) => value,
        Err(error) => {
            warn!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                "[persistence] Failed to parse JSON for resource '{}': {}",
                std::any::type_name::<T>(),
                error
            );
            return false;
        }
    };
    let Ok(default_json) = serde_json::to_value(T::default()) else {
        return false;
    };
    match serde_json::from_value::<T>(merge_json(default_json, snapshot_json)) {
        Ok(value) => {
            world.insert_resource(value);
            true
        }
        Err(error) => {
            warn!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                "[persistence] Failed to deserialize resource '{}': {}. Value skipped.",
                std::any::type_name::<T>(),
                error
            );
            false
        }
    }
}

/// Encode a foreign resource's payload verbatim, as a JSON array of bytes.
///
/// A foreign resource has no Rust type to interpret its bytes through, and it
/// needs none: its payload is blittable and its layout is the whole of what it
/// is. Writing it as JSON rather than raw keeps one snapshot format, so a
/// caller never has to know which lane an entry came from.
pub(super) fn serialize_foreign_resource(resource: &ErasedResource) -> Vec<u8> {
    let bytes: Vec<serde_json::Value> = resource
        .bytes()
        .iter()
        .map(|byte| serde_json::Value::from(*byte))
        .collect();
    serde_json::to_vec(&serde_json::Value::Array(bytes)).unwrap_or_else(|_| b"[]".to_vec())
}

/// Decode the byte array [`serialize_foreign_resource`] wrote.
///
/// `None` unless the payload is an array of exactly `expected_size` byte-valued
/// numbers, because anything else cannot be laid over the declared layout
/// without inventing bytes.
pub(super) fn decode_foreign_resource_payload(
    payload: &[u8],
    expected_size: usize,
) -> Option<Vec<u8>> {
    let parsed: serde_json::Value = serde_json::from_slice(payload).ok()?;
    let array = parsed.as_array()?;
    if array.len() != expected_size {
        return None;
    }
    array
        .iter()
        .map(|value| u8::try_from(value.as_u64()?).ok())
        .collect()
}
