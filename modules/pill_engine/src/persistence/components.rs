//! Component-side persistence: the per-type metadata the reload compares.
//!
//! # Responsibilities
//!
//! - Defines [`PersistTypeMetadata`] and [`PersistTypeManifestEntry`], which
//!   the reload transaction compares before and after the swap to decide which
//!   component types need migrating.
//! - Registers the per-type serialize/deserialize/insert glue that selective
//!   migration reads and writes rows through.
//!
//! # Design
//!
//! The glue is keyed by name rather than by `TypeId`, because a rebuilt image
//! gives every type a fresh `TypeId` for the same name and the name is the only
//! thing two generations agree on. Registration evicts a superseded
//! generation's entries so a name resolves to exactly one live registration.

use super::*;

/// One persistable component type's registration, as the reload sees it.
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

// =============================================================================
// World — Persistable Component Registration
// =============================================================================

impl World {
    /// Register a component type that supports persistence and schema migration.
    ///
    /// In addition to the normal component registration (bit index, storage
    /// factory), this stores serialize/deserialize/insert function pointers so
    /// the engine can migrate this component type during hot-reload.
    ///
    /// The type `T` must implement `serde::Serialize + serde::DeserializeOwned`
    /// so that JSON can round-trip its data.  When the struct shape changes
    /// between reloads, serde matches fields by **name** — new fields get
    /// `Default::default()`, removed fields are silently ignored.
    ///
    /// The schema hash of this path covers the type name, its size, and the
    /// kind of every field in a default instance. A type registered through
    /// the derive uses [`Self::register_persistable_component_with_layout`]
    /// instead, whose hash additionally covers the declared field layout, so
    /// two shapes with identical defaults but different containers (`Vec<f32>`
    /// against `Vec<String>`) can never compare equal.
    pub fn register_persistable_component<T>(&mut self)
    where
        T: Component + Serialize + DeserializeOwned + Default + 'static,
    {
        self.register_persistable_component_inner::<T>(&[]);
    }

    /// Shared registration body for the layout-less and layout-carrying entry
    /// points.
    ///
    /// `fields` is the compile-time field layout when the caller has one; the
    /// schema hash incorporates it so a container kind or element type change
    /// forces a migration instead of taking the unchanged-schema fast path.
    pub(super) fn register_persistable_component_inner<T>(
        &mut self,
        fields: &'static [crate::component_registry::ComponentFieldDescriptor],
    ) where
        T: Component + Serialize + DeserializeOwned + Default + 'static,
    {
        let component_id = ComponentId::of::<T>();
        // The persist maps are keyed by name and resolved against the name the
        // registry recorded, so both must use the same one: a shared
        // component's declared name, an ordinary component's Rust path. Both
        // of these answer before any registration exists, which is what lets
        // the collision guard run first.
        let type_name = crate::component::ComponentRegistry::registered_name::<T>();
        // Whether a predecessor was announced as superseded is consumed by the
        // collision guard below, so the layout guard remembers it first.
        let superseding = self.superseded_persist_names.contains(&type_name);

        // Step 1: Refuse a name collision before the registration happens.
        // This guard used to run *after* `register_component_inner`, which its
        // `return` then left half-applied: a fresh bit, a name entry, a
        // storage factory pointing into the image about to be discarded and a
        // registration-log entry all survived, so the name resolved to two
        // ids and every retry consumed one bit toward the 128-type ceiling.
        if let Some((existing_id, live_rows)) =
            self.live_component_with_name(&type_name, component_id)
        {
            // One of the names the host announced is the predecessor, not a
            // peer: the reload is replacing this generation, and it rehomes the
            // rows afterwards. Live rows alone could not tell the two apart - a
            // generation being retired still holds them - which is why the
            // announcement exists and why it is consumed here.
            if !self.superseded_persist_names.remove(&type_name) {
                self.record_registration_error(WorldError::ComponentNameCollision {
                    type_name: type_name.clone(),
                    existing_id,
                    incoming_id: component_id,
                    live_rows,
                });
                error!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    type_name = %type_name,
                    live_rows,
                    message = COMPONENT_NAME_COLLISION_REFUSAL
                );
                return;
            }
        }

        // Step 1b: Refuse a layout the predecessor's columns cannot host.
        //
        // Migration reads the rows the incoming generation spawned into the
        // old column through the incoming type, in slots `existing.size`
        // bytes apart inside a buffer sized and aligned for the old layout.
        // Every slot address must therefore stay validly aligned for the
        // incoming type: its alignment may not exceed the buffer's, and the
        // old stride must be a multiple of it. A size change that keeps the
        // alignment - adding an `f32` field, the shape the add-a-field
        // reloads use - is still migratable and passes; widening a field to
        // `f64` is not, and letting it through aborts debug hosts in the
        // migration.
        //
        // Every same-name registration is checked rather than one: older
        // generations can still be listed under the name, and the migration
        // picks the predecessor by name, so any column that cannot host the
        // incoming rows is a reason to refuse.
        if superseding {
            let incoming_size = std::mem::size_of::<T>();
            let incoming_align = std::mem::align_of::<T>();
            for existing_id in self.component_ids_with_name(&type_name) {
                if let Some(existing_layout) = self.component_registry.get_layout(&existing_id) {
                    if incoming_align > existing_layout.align
                        || existing_layout.size % incoming_align != 0
                    {
                        self.record_registration_error(WorldError::ComponentLayoutChanged {
                            type_name: type_name.clone(),
                            existing_size: existing_layout.size,
                            existing_align: existing_layout.align,
                            incoming_size,
                            incoming_align,
                        });
                        error!(
                            target: pill_core::telemetry::telemetry_target::ECS,
                            type_name = %type_name,
                            existing_size = existing_layout.size,
                            existing_align = existing_layout.align,
                            incoming_size,
                            incoming_align,
                            message = COMPONENT_LAYOUT_CHANGED_REFUSAL
                        );
                        return;
                    }
                }
            }
        }

        // Step 2: Perform the standard component registration (bit index,
        // storage factory), carrying the field layout so the registry
        // can check a repeat registration against the first one.
        self.register_component_inner::<T>(fields);

        // Step 3: Purge stale persist entries left over from previous
        // registrations of the same type name.  This handles the case where
        // a component struct is changed and then changed back — the compiler
        // may assign the same TypeId, but old entries from intermediate
        // shapes still pollute the persist maps.
        //
        // Eviction is what makes name resolution unambiguous later, so it must
        // only ever remove a *superseded* generation. A same-name entry whose
        // column still holds rows is not superseded - it is a concurrent peer,
        // registered by another binary that linked the same component type and
        // therefore got its own `TypeId` for it. Evicting that entry would drop
        // its inserter, and every row it owns would be silently discarded at
        // the next reload, so the collision is reported instead.
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

        // Step 4: Store the fresh monomorphized serialize, deserialize, and
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

        let schema_hash = calculate_schema_hash::<T>(fields);
        self.persist_schema_hashes
            .insert(type_name.clone(), schema_hash);

        // Record the registration chronologically so the host can enumerate
        // exactly which types one module's init registered, which is how a
        // component type dropped from a reloaded module is detected.
        self.persist_registration_log
            .push((type_name, self.persist_registration_sequence));
        self.persist_registration_sequence += 1;
    }

    /// Announce the persistable type names a retiring generation registered, so
    /// the init pass that follows may replace their persist entries instead of
    /// being refused as a concurrent peer.
    ///
    /// A reloaded project re-registers every type its previous `init`
    /// registered, and the rebuilt image gives each of them a fresh `TypeId`
    /// for the same name - indistinguishable, at the registration site, from
    /// another binary that linked the same type. The host is the only side that
    /// knows which it is, because it captured these names from the generation
    /// it is retiring, so it says so here just before init.
    ///
    /// The announcement covers one init pass: a mark is consumed by the
    /// registration it applies to, and
    /// [`Self::clear_superseded_persist_registrations`] drops what the pass did
    /// not re-register - a name the new generation no longer declares was
    /// forgotten, not superseded, and must not license a later peer.
    pub fn supersede_persist_registrations(&mut self, type_names: &[String]) {
        self.superseded_persist_names
            .extend(type_names.iter().cloned());
    }

    /// Drop the marks left by [`Self::supersede_persist_registrations`].
    pub fn clear_superseded_persist_registrations(&mut self) {
        self.superseded_persist_names.clear();
    }

    /// Register a persistable component together with its compile-time field
    /// layout, so the C# mirror codegen can emit a typed struct and the schema
    /// hash can account for container fields. See
    /// [`Self::register_component_with_layout`].
    ///
    /// The layout is what distinguishes two component shapes whose defaults
    /// look identical to serde: `Vec<f32>` and `Vec<String>` both serialize a
    /// default instance as `[]`, but their tags differ, so a change of element
    /// type changes the hash and the reload migrates the stored values rather
    /// than reinterpreting the old buffer under the new layout.
    pub fn register_persistable_component_with_layout<T>(
        &mut self,
        fields: &'static [crate::component_registry::ComponentFieldDescriptor],
    ) where
        T: Component + Serialize + DeserializeOwned + Default + 'static,
    {
        self.register_persistable_component_inner::<T>(fields);
        self.component_field_layouts.insert(
            ComponentId::of::<T>(),
            crate::world::ComponentFieldLayout::from_static(fields),
        );
    }
}

// =============================================================================
// Per-Type Monomorphized Functions
// =============================================================================

/// Serialize a single component at `index` from storage into JSON bytes.
pub(super) fn serialize_component<T>(storage: &ComponentColumns, index: usize) -> Vec<u8>
where
    T: Component + Serialize,
{
    let typed_storage = storage.column_of::<T>();
    let value: &T = typed_storage.get::<T>(index);
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
pub(super) fn deserialize_component<T>(bytes: &[u8]) -> Option<Box<dyn Component>>
where
    T: Component + Serialize + DeserializeOwned + Default + 'static,
{
    // Step 1: Deserialize the snapshot bytes into a generic JSON Value.
    let snapshot_json: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| {
            warn!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
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
            warn!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
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
            warn!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                "[persistence] Failed to deserialize '{}': {}. Component data skipped.",
                std::any::type_name::<T>(),
                error
            );
            None
        }
    }
}

/// Downcast and push a boxed component into the concrete column.
pub(super) fn insert_boxed_component<T>(
    storage: &mut ComponentColumns,
    component: Box<dyn Component>,
) where
    T: Component + 'static,
{
    // SAFETY: `raw` is produced by `Box::into_raw` above, so it is valid,
    // correctly aligned, and uniquely owned with no aliasing references
    // outstanding, and `Box::from_raw` takes ownership back exactly once.
    // The `*mut T` cast is sound because the concrete type of `component`
    // is guaranteed to match `T`: the caller resolves this function pointer
    // via the type-name lookup in `migrate_single_component_type`, so the fat
    // pointer's data and vtable are valid for `T`.  `*typed` is moved out
    // and pushed into storage, ending the box's ownership without a
    // double-free.
    let raw = Box::into_raw(component);
    // SAFETY: `raw` was produced by `Box::into_raw(component)` immediately
    // above, so it is valid, aligned, and uniquely owned; the detailed
    // justification (matching concrete type, single ownership handover) is
    // above this function's first use.
    let typed: Box<T> = unsafe { Box::from_raw(raw as *mut T) };
    storage.column_of_mut::<T>().push::<T>(*typed);
}
