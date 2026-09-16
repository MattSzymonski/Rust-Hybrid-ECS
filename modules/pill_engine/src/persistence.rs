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
//! | `serialize`     | `fn(&ComponentColumns, index) → Vec<u8>` | Read concrete component from archetype column, JSON-encode it |
//! | `deserialize`   | `fn(&[u8]) → Option<Box<dyn Component>>` | Decode JSON bytes back into a component; returns None on schema mismatch |
//! | `insert_boxed`  | `fn(&mut ComponentColumns, Box<dyn Component>)` | Downcast and push into the archetype's column |
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
use pill_core::{debug, error, info, warn};
use serde::{de::DeserializeOwned, Serialize};

// Current crate
use crate::archetype::ComponentColumns;
use crate::component::{Component, ComponentId};
use crate::entity::Entity;
use crate::error::{
    PersistenceError, WorldError, COMPONENT_LAYOUT_CHANGED_REFUSAL,
    COMPONENT_NAME_COLLISION_REFUSAL,
};
use crate::resource::{ErasedResource, Resource, ResourceId};
use crate::world::World;

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
// ComponentSnapshot
// =============================================================================

/// One component recovered from a snapshot, in whichever form its lane uses.
///
/// The native lane produces a boxed Rust value that an inserter downcasts; the
/// descriptor lane produces the row bytes directly, because there is no Rust
/// type to box and the column stores bytes anyway.
enum RestoredComponent {
    /// A Rust value, to be placed by its registered inserter.
    Native(Box<dyn Component>),
    /// A complete row image, ready to push into the column.
    Descriptor(Vec<u8>),
}

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
/// };
/// assert_eq!(snapshot.entity_count(), 1);
/// ```
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

impl ComponentSnapshot {
    /// Number of entity snapshots stored.
    pub fn entity_count(&self) -> usize {
        self.entries.len()
    }
}

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
    ///
    /// The schema hash of this path covers the type name, its size, and the
    /// kind of every field in a default instance. A type registered through
    /// the derive uses [`Self::register_persistable_component_with_layout`]
    /// instead, whose hash additionally covers the declared field layout, so
    /// two shapes with identical defaults but different containers (`Vec<f32>`
    /// against `Vec<String>`) can never compare equal.
    pub fn register_persistable_component<T>(&mut self)
    where
        T: Component + Clone + Serialize + DeserializeOwned + Default + 'static,
    {
        self.register_persistable_component_inner::<T>(&[]);
    }

    /// Shared registration body for the layout-less and layout-carrying entry
    /// points.
    ///
    /// `fields` is the compile-time field layout when the caller has one; the
    /// schema hash incorporates it so a container kind or element type change
    /// forces a migration instead of taking the unchanged-schema fast path.
    fn register_persistable_component_inner<T>(
        &mut self,
        fields: &'static [crate::component_registry::ComponentFieldDescriptor],
    ) where
        T: Component + Clone + Serialize + DeserializeOwned + Default + 'static,
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
        // storage factory, copier), carrying the field layout so the registry
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
        T: Component + Clone + Serialize + DeserializeOwned + Default + 'static,
    {
        self.register_persistable_component_inner::<T>(fields);
        self.component_field_layouts.insert(
            ComponentId::of::<T>(),
            crate::world::ComponentFieldLayout::Static(fields),
        );
    }
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
    fn restore_foreign_resource(&mut self, type_name: &str, payload: &[u8]) -> bool {
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

        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            "[persistence] Snapshotting {} entities across {} archetypes ({} persistable component types)...",
            total_entities, total_archetypes, persistable_type_count,
        );

        let mut entries: Vec<Vec<(String, Vec<u8>)>> = Vec::with_capacity(total_entities);
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
                    // A descriptor-only component has no serializer - there is
                    // no Rust type to have generated one - so it is serialized
                    // from its field layout instead. Every such component is
                    // persistable: the validated vocabulary is blittable, so
                    // there is nothing a row could hold that cannot be written
                    // down, and no attribute exists for opting out.
                    if !component_id.is_native_storage() {
                        if let Some(bytes) =
                            self.snapshot_descriptor_row(archetype, *component_id, entity_index)
                        {
                            let type_name = self
                                .component_registry
                                .get_name(component_id)
                                .unwrap_or("?")
                                .to_string();
                            component_data.push((type_name, bytes));
                            total_components_serialized += 1;
                        } else {
                            skipped_non_persistable += 1;
                        }
                        continue;
                    }
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
                            debug!(
                                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
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

        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            "[persistence] Snapshot complete: {} entities, {} components serialized ({} non-persistable skipped)",
            entries.len(),
            total_components_serialized,
            skipped_non_persistable,
        );

        ComponentSnapshot { entries }
    }

    /// Rebuild a descriptor-only component's row image from snapshot bytes.
    ///
    /// Returns `None` when the component has no field layout or the snapshot
    /// payload is not an object, which the caller counts as a failed restore
    /// for that component rather than for the whole entity.
    fn descriptor_restore_image(&self, component_id: ComponentId, json: &[u8]) -> Option<Vec<u8>> {
        let (size, _) = self.component_layout(component_id)?;
        let fields = self.component_field_layout(component_id)?;
        crate::component_field::descriptor_row_image(json, fields, size)
    }

    /// Serialize one descriptor-only component row for the snapshot.
    ///
    /// Returns `None` when the component has no registered field layout, which
    /// is the one shape that cannot be written down: an opaque blob whose bytes
    /// nothing describes would restore into a row nothing could place.
    fn snapshot_descriptor_row(
        &self,
        archetype: &crate::archetype::Archetype,
        component_id: ComponentId,
        row: usize,
    ) -> Option<Vec<u8>> {
        let fields = self.component_field_layout(component_id)?;
        if fields.is_empty() {
            return None;
        }
        let row_bytes = archetype.component_storages.get(component_id)?.bytes(row)?;
        Some(crate::component_field::serialize_descriptor_row(
            row_bytes, fields,
        ))
    }

    /// Destroy all entities and recreate them from a snapshot.
    ///
    /// This is called after a hot-reload: the old component types have been
    /// replaced by new ones (with potentially different `TypeId`s), but the
    /// snapshot still holds data keyed by type **name**.  Matching is done
    /// by name - components whose type name is not found in the new
    /// registration are silently dropped (the component type was removed).
    ///
    /// Deserialization uses JSON, so field additions (serde fills
    /// `Default::default()`) and field removals (serde ignores unknown
    /// keys) are handled gracefully.  Incompatible changes (e.g. changing
    /// a field's type from `f32` to `String`) cause that component to be
    /// skipped with a warning.
    pub fn restore_from_snapshot(&mut self, snapshot: &ComponentSnapshot) {
        let snapshot_entity_count = snapshot.entries.len();
        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
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
        debug!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            "[persistence]   Destroyed {} old entities (stale TypeIds)",
            destroyed_count,
        );

        // Step 2: Recreate entities from snapshot data.
        let mut restored_entity_count: usize = 0;
        let mut restored_component_total: usize = 0;
        let mut skipped_type_removed: usize = 0;
        let mut skipped_deser_fail: usize = 0;
        let mut skipped_no_inserter: usize = 0;

        for (entry_idx, component_set) in snapshot.entries.iter().enumerate() {
            let mut restored_components: Vec<(ComponentId, RestoredComponent)> = Vec::new();
            let mut restored_component_ids: Vec<ComponentId> = Vec::new();

            for (type_name, bytes) in component_set {
                // A descriptor-only component is rebuilt from its own field
                // layout, by name, with anything the snapshot does not carry
                // left at its zero bytes.
                if let Some(component_id) = self
                    .resolve_component_id_by_name_any(type_name)
                    .unwrap_or(None)
                    .filter(|id| !id.is_native_storage())
                {
                    match self.descriptor_restore_image(component_id, bytes) {
                        Some(image) => {
                            restored_components
                                .push((component_id, RestoredComponent::Descriptor(image)));
                            restored_component_ids.push(component_id);
                        }
                        None => skipped_deser_fail += 1,
                    }
                    continue;
                }
                if let Some(&deserialize_fn) = self.persist_deserializers.get(type_name) {
                    if let Some(component) = deserialize_fn(bytes) {
                        if let Some(component_id) =
                            self.resolve_component_id_by_name_logged(type_name)
                        {
                            if entry_idx < 3 {
                                debug!(
                                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                                    "[persistence]   restore '{}' → ok ({} bytes)",
                                    type_name,
                                    bytes.len(),
                                );
                            }
                            restored_components
                                .push((component_id, RestoredComponent::Native(component)));
                            restored_component_ids.push(component_id);
                        } else {
                            skipped_no_inserter += 1;
                            if entry_idx < 3 {
                                debug!(
                                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                                    "[persistence]   restore '{}' → SKIP (no inserter)",
                                    type_name,
                                );
                            }
                        }
                    } else {
                        skipped_deser_fail += 1;
                        debug!(
                            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                            "[persistence]   restore '{}' → SKIP (deserialize failed)",
                            type_name,
                        );
                    }
                } else {
                    skipped_type_removed += 1;
                    if entry_idx < 3 {
                        debug!(
                            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
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
                    match component {
                        RestoredComponent::Native(component) => {
                            if let Some(&insert_fn) = self.persist_inserters.get(&component_id) {
                                insert_fn(&mut archetype.component_storages, component);
                                restored_component_total += 1;
                            }
                        }
                        // The descriptor lane needs no inserter: the row is
                        // already the bytes the column stores, so it is pushed
                        // straight in. `get_or_create_archetype` above built
                        // the column from the same registered layout, so the
                        // width always matches.
                        RestoredComponent::Descriptor(image) => {
                            if let Some(column) = archetype.component_storages.get_mut(component_id)
                            {
                                if column.push_bytes(&image).is_ok() {
                                    restored_component_total += 1;
                                }
                            }
                        }
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

        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            "[persistence] Restore complete: {} entities, {} components inserted",
            restored_entity_count, restored_component_total,
        );
        if skipped_type_removed > 0 {
            debug!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                "[persistence]   {} components skipped (type removed from project)",
                skipped_type_removed,
            );
        }
        if skipped_deser_fail > 0 {
            debug!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                "[persistence]   {} components skipped (deserialization failed)",
                skipped_deser_fail,
            );
        }
        if skipped_no_inserter > 0 {
            debug!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                "[persistence]   {} components skipped (no inserter for TypeId)",
                skipped_no_inserter,
            );
        }
        debug!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            "[persistence]   World now has {} entities in {} archetypes",
            self.entity_locations.len(),
            self.archetypes.len(),
        );
    }

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
    fn resolve_component_id_by_name(
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
    fn resolve_component_id_by_name_logged(&self, type_name: &str) -> Option<ComponentId> {
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
    pub fn drop_forgotten_component_ids(&mut self, component_ids: &[ComponentId]) -> usize {
        let mut dropped_entities = 0;
        for &component_id in component_ids {
            if !component_id.is_native_storage() {
                continue;
            }
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
    fn retire_registration(&mut self, component_id: ComponentId) {
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
        self.component_copiers.remove(&component_id);
        self.persist_serializers.remove(&component_id);
        self.persist_inserters.remove(&component_id);
    }

    /// Remove every registration artifact for one forgotten component type so
    /// it stops appearing in the persistable manifest and registry.
    fn forget_component_type(&mut self, type_name: &str) {
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

    /// Migrate only changed persistable components.
    ///
    /// For each changed type name, this uses the old serializer (captured before
    /// reload) and the new deserializer/inserter (registered by new project_init)
    /// to rewrite only the affected component columns.
    ///
    /// `pre_swap_entities` is the set of entities that existed before the
    /// incoming generation's `init` ran (see [`Self::capture_live_entities`]).
    /// Entities outside the set were spawned by the new generation: they
    /// already carry the current schema, so where a column rebuild touches
    /// them they are round-tripped through the current serializer rather than
    /// the retiring generation's. Pass `None` when every entity predates the
    /// migration (for example when restoring a snapshot).
    pub fn migrate_changed_persistable_components(
        &mut self,
        previous_metadata_by_name: &HashMap<String, PersistTypeMetadata>,
        changed_type_names: &HashSet<String>,
        pre_swap_entities: Option<&HashSet<Entity>>,
    ) -> SelectiveMigrationReport {
        let mut report = SelectiveMigrationReport::default();

        // Sort references into the names rather than cloning every string into
        // a heap `Vec<String>`: the ordering exists only for a deterministic
        // report, and the names are unique, so `sort_unstable` is fine. The
        // sort is skipped entirely when nothing will render the report.
        let mut sorted_changed_type_names: Vec<&str> =
            changed_type_names.iter().map(String::as_str).collect();
        if tracing::enabled!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            tracing::Level::INFO
        ) {
            sorted_changed_type_names.sort_unstable();
        }

        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            "[persistence] Selective migration starting for {} component type(s)...",
            sorted_changed_type_names.len(),
        );

        for type_name in &sorted_changed_type_names {
            let Some(previous_metadata) = previous_metadata_by_name.get(*type_name) else {
                debug!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    "[persistence]   '{}' -> SKIP (missing previous metadata)",
                    type_name,
                );
                report.skipped_type_names.push((*type_name).to_string());
                continue;
            };

            let current_schema_hash = self
                .persist_schema_hashes
                .get(*type_name)
                .copied()
                .unwrap_or(0);
            info!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                "[persistence]   '{}' -> migrating (schema {} -> {})",
                type_name, previous_metadata.schema_hash, current_schema_hash,
            );

            match self.migrate_single_component_type(
                type_name,
                previous_metadata,
                pre_swap_entities,
            ) {
                Ok(migrated_entity_count_for_type) => {
                    info!(
                        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                        "[persistence]   '{}' -> OK ({} entities)",
                        type_name, migrated_entity_count_for_type,
                    );
                    report.migrated_type_count += 1;
                    report.migrated_entity_count += migrated_entity_count_for_type;
                }
                Err(error) => {
                    debug!(
                        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                        "[persistence]   '{type_name}' -> SKIP ({error})",);
                    report.skipped_type_names.push((*type_name).to_string());
                }
            }
        }

        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
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
        pre_swap_entities: Option<&HashSet<Entity>>,
    ) -> Result<usize, PersistenceError> {
        let new_component_id = match self.resolve_component_id_by_name(type_name) {
            Ok(Some(component_id)) => component_id,
            Ok(None) => {
                return Err(PersistenceError::ComponentTypeUnregistered {
                    type_name: type_name.to_string(),
                })
            }
            Err(WorldError::ComponentNameAmbiguous { count, .. }) => {
                return Err(PersistenceError::ComponentTypeAmbiguous {
                    type_name: type_name.to_string(),
                    count,
                })
            }
            Err(_) => {
                return Err(PersistenceError::ComponentTypeUnregistered {
                    type_name: type_name.to_string(),
                })
            }
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
            debug!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                "[persistence]     strategy: in-place column swap for '{}'",
                type_name,
            );
            // The current serializer reads spawns' values with the layout the
            // new generation wrote them in; without it the rebuild would have
            // to misread them through the retiring generation's layout.
            let Some(&serialize_current_component) =
                self.persist_serializers.get(&new_component_id)
            else {
                return Err(PersistenceError::SerializerMissing {
                    type_name: type_name.to_string(),
                });
            };
            self.migrate_component_column_in_place(
                previous_metadata.component_id,
                previous_metadata.serializer,
                serialize_current_component,
                deserialize_component,
                insert_component,
                pre_swap_entities,
            )
        } else {
            debug!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
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
    /// Entities the incoming generation spawned during `init` are part of the
    /// same archetype when the schema change kept the component's layout
    /// identical (a widened `Vec<f32>` element type, for example). Those
    /// already hold current-layout values, so they are serialized with
    /// `serialize_current_component` instead of the retiring generation's
    /// serializer, which would reinterpret their bytes under the old shape.
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
        serialize_current_component: SerializeComponentFn,
        deserialize_new_component: DeserializeComponentFn,
        insert_new_component: InsertComponentFn,
        pre_swap_entities: Option<&HashSet<Entity>>,
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
            // Step 2: Serialize every value before the storage column is
            // removed. Entities spawn-swapped in by the new generation (they
            // are not in the pre-swap set) already hold current-layout values
            // and go through the current serializer.
            let serialized_components: Vec<Vec<u8>> = {
                let Some(archetype) = self.archetypes.get(&archetype_id) else {
                    continue;
                };

                (0..archetype.entities.len())
                    .map(|entity_index| {
                        let spawned_this_generation = pre_swap_entities.is_some_and(|entities| {
                            !entities.contains(&archetype.entities[entity_index])
                        });
                        if spawned_this_generation {
                            serialize_current_component(&archetype.component_storages, entity_index)
                        } else {
                            serialize_old_component(&archetype.component_storages, entity_index)
                        }
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

            if !component_id.is_native_storage() {
                return Err(PersistenceError::NativeStorageExpected { component_id });
            }
            // Drop the old column through the glue that wrote it. The re-home
            // pass already stamped the arriving generation's table onto every
            // column, and old-layout values must not be released by code that
            // never allocated them; `register_component_inner` captured the
            // retiring table when the incoming registration replaced it.
            if let Some(retiring_ops) = self.retired_native_storage_ops.get(&component_id) {
                if let Some(column) = archetype.component_storages.get_mut(component_id) {
                    column.refresh_ops(*retiring_ops);
                }
            }
            if archetype.component_storages.remove(component_id).is_none() {
                return Err(PersistenceError::StorageRemovalFailed { component_id });
            }

            let Some(factory) = self.storage_factories.get(&component_id) else {
                return Err(PersistenceError::StorageFactoryMissing { component_id });
            };
            let crate::archetype::StorageFactory::Native(info) = factory else {
                return Err(PersistenceError::NativeStorageExpected { component_id });
            };
            // Build the replacement column as a concrete erased column (no
            // trait-object vtable) so it stays valid across module unloads.
            archetype.component_storages.insert(
                component_id,
                crate::archetype::ComponentColumn::from_native_info(*info, 0)
                    .expect("a registered native layout must describe an allocation"),
            );

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
fn serialize_component<T>(storage: &ComponentColumns, index: usize) -> Vec<u8>
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
fn deserialize_component<T>(bytes: &[u8]) -> Option<Box<dyn Component>>
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
///
/// `fields` is the declared field layout when the type was registered through
/// the derive, and empty for hand-registered types. Including it matters:
/// normalized default values describe a `Vec<f32>` and a `Vec<String>` alike
/// as an empty array, so without the tags a reload could take the
/// unchanged-schema fast path and then interpret one as the other.
fn calculate_schema_hash<T>(fields: &[crate::component_registry::ComponentFieldDescriptor]) -> u64
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

/// Compute the schema hash for one persistable resource type.
///
/// Mirrors [`calculate_schema_hash`] minus the field-layout fold, which has no
/// resource counterpart: a Rust resource is not a column and declares no
/// blittable field table. A shared resource that *does* declare one publishes
/// it as [`Resource::shared_schema_hash`], which the claim check compares
/// separately - that hash guards cross-artifact agreement about a layout, while
/// this one guards migration across a rebuild, so they are deliberately not the
/// same number.
fn calculate_resource_schema_hash<T>() -> u64
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
fn serialize_resource<T>(world: &World) -> Option<Vec<u8>>
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
fn restore_resource<T>(world: &mut World, bytes: &[u8]) -> bool
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
fn serialize_foreign_resource(resource: &ErasedResource) -> Vec<u8> {
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
fn decode_foreign_resource_payload(payload: &[u8], expected_size: usize) -> Option<Vec<u8>> {
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

/// Downcast and push a boxed component into the concrete VecStorage.
fn insert_boxed_component<T>(storage: &mut ComponentColumns, component: Box<dyn Component>)
where
    T: Component + 'static,
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
    // SAFETY: `raw` was produced by `Box::into_raw(component)` immediately
    // above, so it is valid, aligned, and uniquely owned; the detailed
    // justification (matching concrete type, single ownership handover) is
    // above this function's first use.
    let typed: Box<T> = unsafe { Box::from_raw(raw as *mut T) };
    storage.column_of_mut::<T>().push::<T>(*typed);
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
    use crate::archetype::Blittability;

    #[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
    struct DropTestForgottenComponent {
        value: u32,
    }
    impl Component for DropTestForgottenComponent {}
    trait_type_map::impl_trait_accessible!(dyn Component; DropTestForgottenComponent);

    #[derive(Clone, Debug)]
    struct DropTestKeptComponent {
        value: u32,
    }
    impl Component for DropTestKeptComponent {}
    trait_type_map::impl_trait_accessible!(dyn Component; DropTestKeptComponent);

    /// A distinct type that *declares* another type's name, standing in for the
    /// registration a rebuilt image makes: same name, fresh `TypeId`.
    #[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
    struct DropTestSupersedingComponent {
        value: u32,
    }
    impl Component for DropTestSupersedingComponent {
        fn shared_name() -> Option<&'static str> {
            Some(std::any::type_name::<DropTestForgottenComponent>())
        }
    }
    trait_type_map::impl_trait_accessible!(dyn Component; DropTestSupersedingComponent);

    /// A persistable component whose column is 8 bytes / align 4.
    #[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
    struct LayoutHostComponent {
        a: u32,
        b: u32,
    }
    impl Component for LayoutHostComponent {}
    trait_type_map::impl_trait_accessible!(dyn Component; LayoutHostComponent);

    /// Declares the host's name while widening `b` to `f64` - the f32-to-f64
    /// shape that registers 16 bytes / align 8 over the host's column.
    #[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
    struct LayoutWidenedComponent {
        a: u32,
        b: f64,
    }
    impl Component for LayoutWidenedComponent {
        fn shared_name() -> Option<&'static str> {
            Some(std::any::type_name::<LayoutHostComponent>())
        }
    }
    trait_type_map::impl_trait_accessible!(dyn Component; LayoutWidenedComponent);

    /// Same name, same alignment, one field more: the add-a-field shape that
    /// must keep registering (and migrating).
    #[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
    struct LayoutGrownComponent {
        a: u32,
        b: u32,
        c: u32,
    }
    impl Component for LayoutGrownComponent {
        fn shared_name() -> Option<&'static str> {
            Some(std::any::type_name::<LayoutHostComponent>())
        }
    }
    trait_type_map::impl_trait_accessible!(dyn Component; LayoutGrownComponent);

    /// The refusal sentence is printed verbatim into host logs and grepped by
    /// humans, so it carries no source-wrap artifacts.
    #[test]
    fn collision_refusal_message_has_no_wrap_spaces() {
        assert!(
            !COMPONENT_NAME_COLLISION_REFUSAL.contains("  "),
            "{COMPONENT_NAME_COLLISION_REFUSAL}"
        );
        assert!(!COMPONENT_NAME_COLLISION_REFUSAL.contains('\n'));
        assert!(COMPONENT_NAME_COLLISION_REFUSAL.ends_with("persist entries"));
    }

    /// A superseding registration whose layout the old column cannot host is
    /// refused before anything registers: the host rolls the reload back and
    /// the running generation keeps its storage, instead of the migration
    /// reading the new alignment out of the old column's slots.
    #[test]
    fn a_layout_the_old_column_cannot_host_is_refused() {
        let mut world = World::new();
        world.register_persistable_component::<LayoutHostComponent>();
        let host_id = ComponentId::of::<LayoutHostComponent>();
        let type_name = std::any::type_name::<LayoutHostComponent>().to_string();
        let entity = world
            .create_entity()
            .with(LayoutHostComponent { a: 1, b: 2 })
            .build()
            .unwrap();
        let _ = world.take_registration_error();

        // The rebuilt image's registration: same name, wider alignment.
        world.supersede_persist_registrations(std::slice::from_ref(&type_name));
        world.register_persistable_component::<LayoutWidenedComponent>();

        let error = world
            .take_registration_error()
            .expect("a layout the old column cannot host must be refused");
        let message = error.to_string();
        assert!(
            message.contains("was re-registered with a different size"),
            "the refusal keeps the sentence the migration suite greps for: {message}"
        );
        match error {
            WorldError::ComponentLayoutChanged {
                existing_size,
                existing_align,
                incoming_size,
                incoming_align,
                ..
            } => {
                assert_eq!((existing_size, existing_align), (8, 4));
                assert_eq!((incoming_size, incoming_align), (16, 8));
            }
            other => panic!("expected a ComponentLayoutChanged refusal, got {other:?}"),
        }

        // Nothing registered, and the predecessor kept its registration.
        assert_eq!(
            world.component_registry().registered_components().count(),
            1,
            "a refused registration leaves the registry as it was"
        );
        assert_eq!(world.live_row_count(host_id), 1);
        assert!(world.entity_locations.contains_key(&entity));
        assert!(!world
            .storage_factories
            .contains_key(&ComponentId::of::<LayoutWidenedComponent>()));
    }

    /// A size change that keeps the alignment registers: the old column's
    /// slots stay validly aligned for the incoming type, which is what the
    /// add-a-field reload scenarios depend on.
    #[test]
    fn a_size_change_that_keeps_the_alignment_registers() {
        let mut world = World::new();
        world.register_persistable_component::<LayoutHostComponent>();
        let type_name = std::any::type_name::<LayoutHostComponent>().to_string();
        let _ = world.take_registration_error();

        world.supersede_persist_registrations(std::slice::from_ref(&type_name));
        world.register_persistable_component::<LayoutGrownComponent>();

        assert!(
            world.take_registration_error().is_none(),
            "a wider field of the same alignment must register"
        );
        let grown_layout = world
            .component_registry()
            .get_layout(&ComponentId::of::<LayoutGrownComponent>())
            .expect("the successor is registered");
        assert_eq!((grown_layout.size, grown_layout.align), (12, 4));
    }

    /// Dropping a forgotten type removes its columns from every entity while
    /// entities that carry other components survive with those intact.
    #[test]
    fn drop_forgotten_components_removes_only_the_forgotten_data() {
        let mut world = World::new();
        world.register_persistable_component::<DropTestForgottenComponent>();
        world.register_component::<DropTestKeptComponent>();

        let forgotten_id = ComponentId::of::<DropTestForgottenComponent>();
        let kept_id = ComponentId::of::<DropTestKeptComponent>();

        let entity_only_forgotten = world
            .create_entity()
            .with(DropTestForgottenComponent { value: 1 })
            .build()
            .unwrap();
        let entity_with_both = world
            .create_entity()
            .with(DropTestForgottenComponent { value: 2 })
            .with(DropTestKeptComponent { value: 3 })
            .build()
            .unwrap();

        // The data is readable before the drop.
        assert_eq!(
            world
                .get_component::<DropTestForgottenComponent>(entity_with_both)
                .unwrap()
                .value,
            2
        );

        let type_name = std::any::type_name::<DropTestForgottenComponent>().to_string();
        let dropped = world.drop_forgotten_components(std::slice::from_ref(&type_name));
        assert_eq!(dropped, 2, "both entities carried the forgotten component");

        // The entity that only carried the forgotten component is destroyed;
        // the entity that also carries a kept component survives.
        assert!(!world.entity_locations.contains_key(&entity_only_forgotten));
        assert!(world.entity_locations.contains_key(&entity_with_both));
        assert_eq!(world.entity_locations.len(), 1);

        // The surviving entity's archetype keeps the kept component and no
        // longer contains the forgotten one.
        let location = world.entity_locations[&entity_with_both];
        let archetype = &world.archetypes[&location.archetype_id];
        assert!(archetype.component_types.contains(&kept_id));
        assert!(!archetype.component_types.contains(&forgotten_id));

        // The kept component's value survives untouched.
        let kept = world
            .get_component::<DropTestKeptComponent>(entity_with_both)
            .expect("kept component should still be readable");
        assert_eq!(kept.value, 3);

        // The persistable manifest no longer knows the forgotten type.
        assert!(world
            .persist_type_manifest()
            .iter()
            .all(|entry| entry.type_name != type_name));

        // Re-registering the forgotten type works and fresh data can be seeded.
        world.register_persistable_component::<DropTestForgottenComponent>();
        let reseeded = world
            .create_entity()
            .with(DropTestForgottenComponent { value: 9 })
            .build()
            .unwrap();
        assert!(world.entity_locations.contains_key(&reseeded));
    }

    /// Every native id sharing the forgotten name loses its rows, not just the
    /// one a name resolver returns: an ambiguous name resolved to nothing at
    /// all, so the purge used to skip both generations while
    /// `forget_component_type` removed their registrations anyway.
    #[test]
    fn forget_strips_rows_for_every_id_sharing_the_name() {
        let mut world = World::new();
        world.register_persistable_component::<DropTestForgottenComponent>();
        let first_id = ComponentId::of::<DropTestForgottenComponent>();
        let type_name = std::any::type_name::<DropTestForgottenComponent>().to_string();

        let first_entity = world
            .create_entity()
            .with(DropTestForgottenComponent { value: 1 })
            .build()
            .unwrap();
        assert_eq!(world.live_row_count(first_id), 1);

        // A rebuilt image's registration: same name, fresh id, its own rows.
        world.supersede_persist_registrations(std::slice::from_ref(&type_name));
        world.register_persistable_component::<DropTestSupersedingComponent>();
        let second_id = ComponentId::of::<DropTestSupersedingComponent>();
        assert_ne!(second_id, first_id);
        let second_entity = world
            .create_entity()
            .with(DropTestSupersedingComponent { value: 2 })
            .build()
            .unwrap();
        assert_eq!(world.live_row_count(second_id), 1);

        let dropped = world.drop_forgotten_components(std::slice::from_ref(&type_name));
        assert_eq!(
            dropped, 2,
            "both generations' rows are stripped, not just one id's"
        );
        assert!(!world.entity_locations.contains_key(&first_entity));
        assert!(!world.entity_locations.contains_key(&second_entity));
        assert!(!world.storage_factories.contains_key(&first_id));
        assert!(!world.storage_factories.contains_key(&second_id));
        assert!(!world.component_copiers.contains_key(&first_id));
        assert!(!world.component_copiers.contains_key(&second_id));
    }

    /// A refused persistable registration changes nothing: the guard runs
    /// before the registration does, so no bit, name entry, storage factory or
    /// log entry is created for a call that returns without registering.
    #[test]
    fn a_refused_persistable_registration_changes_nothing() {
        let mut world = World::new();
        world.register_persistable_component::<DropTestForgottenComponent>();
        // A peer with live rows under the same name: without the supersede
        // announcement the guard refuses instead of evicting it.
        world.register_persistable_component::<DropTestSupersedingComponent>();
        let peer_id = ComponentId::of::<DropTestSupersedingComponent>();
        let peer = world
            .create_entity()
            .with(DropTestSupersedingComponent { value: 1 })
            .build()
            .unwrap();
        assert_eq!(world.live_row_count(peer_id), 1);
        let _ = world.take_registration_error();

        let registry_count = world.component_registry().registered_components().count();
        for attempt in 0..3 {
            world.register_persistable_component::<DropTestForgottenComponent>();
            let error = world
                .take_registration_error()
                .unwrap_or_else(|| panic!("attempt {attempt} must be refused"));
            assert!(matches!(error, WorldError::ComponentNameCollision { .. }));
            assert_eq!(
                world.component_registry().registered_components().count(),
                registry_count,
                "a refused attempt leaves the registry as it was"
            );
        }
        assert!(world.entity_locations.contains_key(&peer));
        assert_eq!(
            world.live_row_count(peer_id),
            1,
            "the peer keeps its rows across every refused attempt"
        );
    }

    /// The id-keyed purge removes the rows and every registration artifact of
    /// the ids it is given, while a same-name re-registration - the rollback
    /// generation, with a fresh id - keeps the name-keyed entries alive.
    #[test]
    fn stranded_ids_lose_their_rows_and_factories() {
        let mut world = World::new();
        world.register_persistable_component::<DropTestForgottenComponent>();
        let stranded_id = ComponentId::of::<DropTestForgottenComponent>();
        let type_name = std::any::type_name::<DropTestForgottenComponent>().to_string();

        let entity = world
            .create_entity()
            .with(DropTestForgottenComponent { value: 4 })
            .build()
            .unwrap();
        assert_eq!(world.live_row_count(stranded_id), 1);

        // The rollback generation re-registers the same name under a fresh id,
        // exactly as a rebuilt image does.
        world.supersede_persist_registrations(std::slice::from_ref(&type_name));
        world.register_persistable_component::<DropTestSupersedingComponent>();
        let successor_id = ComponentId::of::<DropTestSupersedingComponent>();
        assert_ne!(successor_id, stranded_id);

        let dropped = world.drop_forgotten_component_ids(&[stranded_id]);
        assert_eq!(dropped, 1, "the stranded type's row was removed with it");

        // The failed generation's id holds nothing anywhere...
        assert!(!world.entity_locations.contains_key(&entity));
        assert!(!world.storage_factories.contains_key(&stranded_id));
        assert!(!world.component_copiers.contains_key(&stranded_id));
        assert!(!world.persist_serializers.contains_key(&stranded_id));
        assert!(!world.persist_inserters.contains_key(&stranded_id));

        // ...while the rollback generation's own entries survive, including
        // the name-keyed ones the two registrations share.
        assert!(world.persist_serializers.contains_key(&successor_id));
        assert!(world.persist_inserters.contains_key(&successor_id));
        assert!(world.persist_deserializers.contains_key(&type_name));
        assert!(world.persist_schema_hashes.contains_key(&type_name));
    }

    /// The registration log distinguishes a type that was dropped entirely
    /// from one merely downgraded to a plain component.
    #[test]
    fn registered_component_names_since_covers_plain_components() {
        let mut world = World::new();
        let sequence = world.component_registration_sequence();

        world.register_persistable_component::<DropTestForgottenComponent>();
        world.register_component::<DropTestKeptComponent>();

        let registered = world.registered_component_names_since(sequence);
        assert!(registered
            .iter()
            .any(|name| name.contains("DropTestForgottenComponent")));
        assert!(registered
            .iter()
            .any(|name| name.contains("DropTestKeptComponent")));

        let persistable = world.persist_type_names_registered_since(sequence);
        assert!(persistable
            .iter()
            .any(|name| name.contains("DropTestForgottenComponent")));
        assert!(!persistable
            .iter()
            .any(|name| name.contains("DropTestKeptComponent")));
    }

    /// Re-registering the same persistable type is idempotent across the
    /// persist maps: one serializer/inserter per ComponentId and one
    /// deserializer/schema hash per name. Pins the invariant the unified
    /// registration struct (audit 4.2) must preserve.
    #[test]
    fn persistable_registration_is_idempotent_across_repeated_calls() {
        let mut world = World::new();
        let component_id = ComponentId::of::<DropTestForgottenComponent>();
        world.register_persistable_component::<DropTestForgottenComponent>();
        world.register_persistable_component::<DropTestForgottenComponent>();

        assert_eq!(world.persist_serializers.len(), 1);
        assert!(world.persist_serializers.contains_key(&component_id));
        assert_eq!(world.persist_inserters.len(), 1);
        assert!(world.persist_inserters.contains_key(&component_id));

        let type_name = std::any::type_name::<DropTestForgottenComponent>().to_string();
        assert_eq!(world.persist_deserializers.len(), 1);
        assert!(world.persist_deserializers.contains_key(&type_name));
        assert_eq!(world.persist_schema_hashes.len(), 1);
        assert!(world.persist_schema_hashes.contains_key(&type_name));
    }

    // =========================================================================
    // Concurrent-peer guard
    // =========================================================================

    /// A second registration of a type name whose existing column still holds
    /// rows is a concurrent peer, not a superseded generation, so it is
    /// reported instead of evicting the peer's persist entries and silently
    /// dropping its rows at the next reload.
    #[test]
    fn a_same_name_registration_over_live_rows_is_reported_not_evicted() {
        let mut world = World::new();
        world.register_persistable_component::<DropTestForgottenComponent>();
        let native_id = ComponentId::of::<DropTestForgottenComponent>();
        let type_name = std::any::type_name::<DropTestForgottenComponent>().to_string();

        // Give the native column a live row, which is what makes the second
        // registration a peer rather than a dead generation.
        world
            .create_entity()
            .with(DropTestForgottenComponent { value: 7 })
            .build()
            .unwrap();
        assert_eq!(world.live_row_count(native_id), 1);

        // A second component claiming the same name arrives - the in-process
        // stand-in for a second binary that linked the same type.
        let error = world
            .register_component_descriptor(
                0x5EED,
                type_name.clone(),
                4,
                4,
                0,
                Blittability::engine_verified(),
            )
            .unwrap_err();

        match error {
            WorldError::ComponentNameCollision {
                type_name: reported_name,
                existing_id,
                live_rows,
                ..
            } => {
                assert_eq!(reported_name, type_name);
                assert_eq!(existing_id, native_id);
                assert_eq!(live_rows, 1);
            }
            other => panic!("expected a name collision, got {other:?}"),
        }

        // The peer's persist entries are untouched, so its rows still restore.
        assert!(world.persist_inserters.contains_key(&native_id));
        assert!(world.persist_serializers.contains_key(&native_id));
    }

    /// The same collision is allowed through once the existing column has no
    /// rows left: that is a superseded generation, and replacing it is the
    /// behaviour hot reload depends on.
    #[test]
    fn a_same_name_registration_over_an_empty_column_still_succeeds() {
        let mut world = World::new();
        world.register_persistable_component::<DropTestForgottenComponent>();
        let type_name = std::any::type_name::<DropTestForgottenComponent>().to_string();

        // No entity is created, so the native column holds nothing.
        assert_eq!(
            world.live_row_count(ComponentId::of::<DropTestForgottenComponent>()),
            0
        );

        let result = world.register_component_descriptor(
            0x5EED,
            type_name,
            4,
            4,
            0,
            Blittability::engine_verified(),
        );
        assert!(
            result.is_ok(),
            "an empty same-name column is a dead generation, not a peer: {result:?}"
        );
    }

    /// The same collision shape as the peer test above - same name, different
    /// id, live rows - but announced by the host as the retiring generation.
    /// That registration is the predecessor and must replace the entries, which
    /// is the case every project reload runs through.
    #[test]
    fn an_announced_name_replaces_its_live_predecessor() {
        let mut world = World::new();
        world.register_persistable_component::<DropTestForgottenComponent>();
        let predecessor_id = ComponentId::of::<DropTestForgottenComponent>();
        let type_name = std::any::type_name::<DropTestForgottenComponent>().to_string();
        world
            .create_entity()
            .with(DropTestForgottenComponent { value: 7 })
            .build()
            .unwrap();
        assert_eq!(world.live_row_count(predecessor_id), 1);

        let successor_id = ComponentId::of::<DropTestSupersedingComponent>();
        world.supersede_persist_registrations(&[type_name]);
        world.register_persistable_component::<DropTestSupersedingComponent>();

        // Replaced, not refused: the successor's entries are the live ones now.
        assert!(world.take_registration_error().is_none());
        assert!(!world.persist_serializers.contains_key(&predecessor_id));
        assert!(world.persist_serializers.contains_key(&successor_id));
        assert!(world.persist_inserters.contains_key(&successor_id));
        // The predecessor's rows are untouched: rehoming them is the reload's
        // step, not the registry's.
        assert_eq!(world.live_row_count(predecessor_id), 1);

        // The announcement was consumed with that one registration. With the
        // successor now holding a row of its own, the same arrival the mark
        // permitted a moment ago is refused again.
        world
            .create_entity()
            .with(DropTestSupersedingComponent { value: 1 })
            .build()
            .unwrap();
        world.register_persistable_component::<DropTestForgottenComponent>();
        assert!(matches!(
            world.take_registration_error(),
            Some(WorldError::ComponentNameCollision { .. })
        ));
    }

    // =========================================================================
    // Retiring drop glue across an in-place migration
    // =========================================================================

    /// Rows dropped by each stand-in's glue, so a migration can be asked
    /// *which* generation's destructor released the old values.
    static RETIRING_GLUE_DROPS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    static ARRIVING_GLUE_DROPS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    /// The retiring generation of a shared persistable type.
    #[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
    struct RetiringGlueComponent {
        #[allow(dead_code)]
        value: u32,
    }
    impl Component for RetiringGlueComponent {
        fn shared_name() -> Option<&'static str> {
            Some("audit::DropGlue")
        }
    }
    impl Drop for RetiringGlueComponent {
        fn drop(&mut self) {
            RETIRING_GLUE_DROPS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    trait_type_map::impl_trait_accessible!(dyn Component; RetiringGlueComponent);

    /// The arriving generation: same declared name and shape, different glue.
    #[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
    struct ArrivingGlueComponent {
        #[allow(dead_code)]
        value: u32,
    }
    impl Component for ArrivingGlueComponent {
        fn shared_name() -> Option<&'static str> {
            Some("audit::DropGlue")
        }
    }
    impl Drop for ArrivingGlueComponent {
        fn drop(&mut self) {
            ARRIVING_GLUE_DROPS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    trait_type_map::impl_trait_accessible!(dyn Component; ArrivingGlueComponent);

    /// The in-place migration drops old-layout rows through the glue that wrote
    /// them. The sequence that used to break it: the arriving generation's
    /// registration replaces the factory, `rehome_native_columns` stamps the
    /// arriving table onto the still-old column, and only then does the
    /// migration consume that column.
    #[test]
    fn in_place_migration_drops_old_rows_through_the_retiring_glue() {
        use std::sync::atomic::Ordering;

        let mut world = World::new();
        world.register_persistable_component::<RetiringGlueComponent>();
        let component_id = ComponentId::of::<RetiringGlueComponent>();
        let type_name =
            crate::component::ComponentRegistry::registered_name::<RetiringGlueComponent>()
                .to_string();

        let entity = world
            .create_entity()
            .with(RetiringGlueComponent { value: 3 })
            .build()
            .unwrap();
        assert!(world.entity_locations.contains_key(&entity));
        world.rehome_native_columns();

        // Capture the retiring generation's serializer while it is still the
        // registered one, then stand in for the rebuilt image's registration.
        let serialize_old = world.persist_serializers[&component_id];
        world.register_persistable_component::<ArrivingGlueComponent>();
        let serialize_current = world.persist_serializers[&component_id];
        let deserialize_new = world.persist_deserializers[&type_name];
        let insert_new = world.persist_inserters[&component_id];

        // The re-home the reload runs before migration stamps the arriving
        // generation's table onto the old column - the state that produced the
        // invalid free.
        world.rehome_native_columns();

        let retiring_before = RETIRING_GLUE_DROPS.load(Ordering::SeqCst);
        let arriving_before = ARRIVING_GLUE_DROPS.load(Ordering::SeqCst);
        let migrated = world
            .migrate_component_column_in_place(
                component_id,
                serialize_old,
                serialize_current,
                deserialize_new,
                insert_new,
                None,
            )
            .expect("the in-place migration runs on a native column");
        assert_eq!(migrated, 1);

        assert_eq!(
            RETIRING_GLUE_DROPS.load(Ordering::SeqCst),
            retiring_before + 1,
            "the old row is released by the generation that allocated it"
        );
        // `deserialize_component` materializes a `T::default()` schema baseline
        // per row, and that baseline is dropped through the arriving type's
        // glue. One row was migrated, so exactly one arriving drop is the
        // baseline - the old row itself must not be among them.
        assert_eq!(
            ARRIVING_GLUE_DROPS.load(Ordering::SeqCst),
            arriving_before + 1,
            "the arriving glue released only its own schema baseline, never the old row"
        );
    }

    /// Re-registering a persistable type while its own column holds rows is
    /// the ordinary hot-reload path and must not trip the peer guard: the
    /// guard only looks at *other* component ids.
    #[test]
    fn re_registering_the_same_type_over_live_rows_is_not_a_collision() {
        let mut world = World::new();
        world.register_persistable_component::<DropTestForgottenComponent>();
        world
            .create_entity()
            .with(DropTestForgottenComponent { value: 1 })
            .build()
            .unwrap();

        world.register_persistable_component::<DropTestForgottenComponent>();
        assert!(
            world.take_registration_error().is_none(),
            "a reload re-registering its own type is not a peer collision"
        );
    }

    // =========================================================================
    // Name resolution
    // =========================================================================

    /// A name claimed by exactly one registration resolves to it, through both
    /// the persistable-filtered resolver and the unfiltered one.
    #[test]
    fn an_unambiguous_name_resolves_through_both_resolvers() {
        let mut world = World::new();
        world.register_persistable_component::<DropTestForgottenComponent>();
        let component_id = ComponentId::of::<DropTestForgottenComponent>();
        let type_name = std::any::type_name::<DropTestForgottenComponent>();

        assert_eq!(
            world.resolve_component_id_by_name(type_name).unwrap(),
            Some(component_id)
        );
        assert_eq!(
            world.resolve_component_id_by_name_any(type_name).unwrap(),
            Some(component_id)
        );
    }

    /// A name claimed by two registrations is reported as ambiguous rather
    /// than resolved by the old `max_by_key(bit)` tiebreak, which is not a
    /// recency ordering and could bind callers to the wrong column.
    #[test]
    fn a_name_claimed_twice_resolves_to_an_ambiguity_error() {
        let mut world = World::new();
        world.register_persistable_component::<DropTestForgottenComponent>();
        let type_name = std::any::type_name::<DropTestForgottenComponent>().to_string();

        // The native column is empty, so a second claim on the name registers
        // (see the empty-column test above) and both are now visible to the
        // unfiltered resolver.
        world
            .register_component_descriptor(
                0x5EED,
                type_name.clone(),
                4,
                4,
                0,
                Blittability::engine_verified(),
            )
            .unwrap();

        let error = world
            .resolve_component_id_by_name_any(&type_name)
            .unwrap_err();
        match error {
            WorldError::ComponentNameAmbiguous {
                type_name: reported_name,
                count,
            } => {
                assert_eq!(reported_name, type_name);
                assert_eq!(count, 2);
            }
            other => panic!("expected an ambiguity error, got {other:?}"),
        }

        // The persistable-filtered resolver still collapses to one, because
        // only the native registration has an inserter.
        assert_eq!(
            world.resolve_component_id_by_name(&type_name).unwrap(),
            Some(ComponentId::of::<DropTestForgottenComponent>())
        );
    }

    /// A name nothing claims resolves to `None` rather than an error - an
    /// unregistered type is an ordinary outcome, not an ambiguity.
    #[test]
    fn an_unclaimed_name_resolves_to_none() {
        let world = World::new();
        assert_eq!(
            world
                .resolve_component_id_by_name("nothing::Registered")
                .unwrap(),
            None
        );
        assert_eq!(
            world
                .resolve_component_id_by_name_any("nothing::Registered")
                .unwrap(),
            None
        );
    }
    // =========================================================================
    // Descriptor-only persistence (C.3)
    // =========================================================================

    /// Register a descriptor-only component with a two-field layout.
    ///
    /// Mirrors what the C# manifest path registers: a blittable row plus the
    /// field descriptors that describe it, which together are everything the
    /// generic codec needs.
    fn register_probe_descriptor_component(world: &mut World, name: &str) -> ComponentId {
        let component_id = world
            .register_component_descriptor(
                0xC3_0001,
                name.to_string(),
                8,
                4,
                77,
                Blittability::engine_verified(),
            )
            .expect("a valid descriptor layout registers");
        world
            .register_component_descriptor_with_layout(
                component_id,
                vec![
                    crate::component_registry::ComponentFieldDescriptor {
                        name: "health",
                        type_tag: "i32",
                        offset: 0,
                        size: 4,
                        align: 4,
                        element_count: 0,
                    },
                    crate::component_registry::ComponentFieldDescriptor {
                        name: "speed",
                        type_tag: "f32",
                        offset: 4,
                        size: 4,
                        align: 4,
                        element_count: 0,
                    },
                ],
            )
            .expect("the layout fits the registered size");
        component_id
    }

    /// A descriptor-only component survives a snapshot and restore.
    ///
    /// Before the descriptor codec existed this component was absent from the
    /// snapshot entirely: `snapshot_components` consulted `persist_serializers`,
    /// which only a Rust type can populate, so a C#-declared component was
    /// silently dropped by a save.
    #[test]
    fn a_descriptor_only_component_round_trips_through_a_snapshot() {
        let mut world = World::new();
        let component_id = register_probe_descriptor_component(&mut world, "probe::Stats");

        let entity = world.create_entity().build().unwrap();
        world
            .add_descriptor_component(entity, component_id, &[7, 0, 0, 0, 0, 0, 160, 64])
            .expect("the row is the registered width");

        let snapshot = world.snapshot_components();
        assert_eq!(snapshot.entries.len(), 1, "the entity reached the snapshot");
        assert_eq!(
            snapshot.entries[0].len(),
            1,
            "its descriptor component reached the snapshot"
        );
        assert_eq!(snapshot.entries[0][0].0, "probe::Stats");

        world.restore_from_snapshot(&snapshot);

        let restored: Vec<Entity> = world.entity_locations.keys().copied().collect();
        assert_eq!(restored.len(), 1, "one entity comes back");
        let location = world.entity_locations[&restored[0]];
        let archetype = &world.archetypes[&location.archetype_id];
        let row = archetype
            .component_storages
            .get(component_id)
            .expect("the restored archetype owns the column")
            .bytes(location.index_in_archetype)
            .expect("the row exists");
        assert_eq!(
            row,
            &[7, 0, 0, 0, 0, 0, 160, 64],
            "every byte of the row survives the round trip"
        );
    }

    /// A field the snapshot does not carry restores to defined bytes.
    #[test]
    fn a_field_added_since_the_snapshot_restores_zeroed() {
        let fields = [
            crate::component_registry::ComponentFieldDescriptor {
                name: "health",
                type_tag: "i32",
                offset: 0,
                size: 4,
                align: 4,
                element_count: 0,
            },
            crate::component_registry::ComponentFieldDescriptor {
                name: "shield",
                type_tag: "i32",
                offset: 4,
                size: 4,
                align: 4,
                element_count: 0,
            },
        ];

        // A snapshot written before `shield` existed.
        let image = crate::component_field::descriptor_row_image(br#"{"health":9}"#, &fields, 8)
            .expect("the payload is an object");

        assert_eq!(&image[0..4], &9i32.to_le_bytes(), "the carried field lands");
        assert_eq!(
            &image[4..8],
            &[0, 0, 0, 0],
            "the added field starts defined rather than reading stale memory"
        );
    }

    /// A reordered field follows its name, not its position.
    #[test]
    fn a_reordered_field_restores_by_name() {
        let before = [
            crate::component_registry::ComponentFieldDescriptor {
                name: "health",
                type_tag: "i32",
                offset: 0,
                size: 4,
                align: 4,
                element_count: 0,
            },
            crate::component_registry::ComponentFieldDescriptor {
                name: "speed",
                type_tag: "i32",
                offset: 4,
                size: 4,
                align: 4,
                element_count: 0,
            },
        ];
        // The same two fields, swapped.
        let after = [
            crate::component_registry::ComponentFieldDescriptor {
                name: "speed",
                type_tag: "i32",
                offset: 0,
                size: 4,
                align: 4,
                element_count: 0,
            },
            crate::component_registry::ComponentFieldDescriptor {
                name: "health",
                type_tag: "i32",
                offset: 4,
                size: 4,
                align: 4,
                element_count: 0,
            },
        ];

        let mut row = Vec::new();
        row.extend_from_slice(&11i32.to_le_bytes());
        row.extend_from_slice(&22i32.to_le_bytes());
        let json = crate::component_field::serialize_descriptor_row(&row, &before);

        let image = crate::component_field::descriptor_row_image(&json, &after, 8)
            .expect("the payload is an object");
        assert_eq!(
            &image[0..4],
            &22i32.to_le_bytes(),
            "speed moved to offset 0"
        );
        assert_eq!(
            &image[4..8],
            &11i32.to_le_bytes(),
            "health moved to offset 4"
        );
    }

    /// A field the codec cannot interpret still survives the round trip.
    ///
    /// The editor refuses to *write* a `struct:` field; persistence must still
    /// *preserve* it, which is why the two paths do not share a rule.
    #[test]
    fn an_uninterpretable_field_round_trips_as_bytes() {
        let fields = [crate::component_registry::ComponentFieldDescriptor {
            name: "nested",
            type_tag: "struct:probe::Inner",
            offset: 0,
            size: 4,
            align: 4,
            element_count: 0,
        }];

        let row = [1u8, 2, 3, 4];
        let json = crate::component_field::serialize_descriptor_row(&row, &fields);
        let image = crate::component_field::descriptor_row_image(&json, &fields, 4)
            .expect("the payload is an object");
        assert_eq!(&image[..], &row[..], "the opaque bytes come back unchanged");
    }

    // =========================================================================
    // Resource persistence
    // =========================================================================

    /// An ordinary project resource: private identity, serde-able shape.
    #[derive(Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
    struct PersistProbeScore {
        points: u32,
        label: String,
    }
    impl crate::resource::Resource for PersistProbeScore {}

    /// A resource registered without persistence, to pin the opt-in.
    #[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
    struct PersistProbeOpaque {
        counter: u32,
    }
    impl crate::resource::Resource for PersistProbeOpaque {}

    /// Two builds of one resource, the second a field wider.
    ///
    /// The test process cannot rebuild itself, so a reload is modelled the way
    /// the component suite models it: two distinct Rust types registered under
    /// one persistence name. A real reload produces exactly this state - the
    /// arriving build is a different type to this process, and the name is the
    /// only thing the two generations agree on.
    ///
    /// Deliberately *not* shared resources: a shared name is claimed by one
    /// declaring type and one layout, so two types could never both hold it.
    /// The persistence name is a separate string for precisely this reason.
    #[derive(Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
    struct PersistProbeSettingsV1 {
        volume: f32,
    }
    impl crate::resource::Resource for PersistProbeSettingsV1 {}

    #[derive(Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
    struct PersistProbeSettingsV2 {
        volume: f32,
        brightness: f32,
    }
    impl crate::resource::Resource for PersistProbeSettingsV2 {}

    /// The persistence name both builds of the settings resource register under.
    const PROBE_SETTINGS_NAME: &str = "probe::Settings";

    /// A registered resource's value survives a snapshot and restore.
    ///
    /// Before this existed, `snapshot_resources` did not exist either: a saved
    /// world came back with every resource missing while looking complete.
    #[test]
    fn a_persistable_resource_round_trips_through_a_snapshot() {
        let mut world = World::new();
        world.register_persistable_resource::<PersistProbeScore>();
        world.insert_resource(PersistProbeScore {
            points: 42,
            label: "wave three".to_string(),
        });

        let snapshot = world.snapshot_resources();
        assert_eq!(snapshot.resource_count(), 1, "the resource was captured");

        // A fresh world stands in for the loading process: it declares the
        // resource but has never been given a value.
        let mut loaded = World::new();
        loaded.register_persistable_resource::<PersistProbeScore>();
        assert!(loaded.get_resource::<PersistProbeScore>().is_none());

        loaded.restore_resources(&snapshot);
        assert_eq!(
            loaded.get_resource::<PersistProbeScore>(),
            Some(&PersistProbeScore {
                points: 42,
                label: "wave three".to_string(),
            }),
            "every field comes back"
        );
    }

    /// Registration is what opts a resource into a snapshot.
    #[test]
    fn an_unregistered_resource_stays_out_of_the_snapshot() {
        let mut world = World::new();
        world.register_persistable_resource::<PersistProbeScore>();
        world.insert_resource(PersistProbeScore::default());
        // Inserted, never registered as persistable.
        world.insert_resource(PersistProbeOpaque { counter: 9 });

        let snapshot = world.snapshot_resources();
        assert_eq!(
            snapshot.resource_count(),
            1,
            "only the registered resource is captured"
        );
        assert!(
            snapshot
                .payload(std::any::type_name::<PersistProbeScore>())
                .is_some(),
            "and it is the one that opted in"
        );
    }

    /// A resource that was never inserted contributes no entry.
    ///
    /// "Registered" and "has a value" are different states, and a snapshot
    /// records values. Writing a null would make the loading generation
    /// overwrite whatever it inserted itself with an empty default.
    #[test]
    fn a_registered_resource_with_no_value_is_absent_from_the_snapshot() {
        let mut world = World::new();
        world.register_persistable_resource::<PersistProbeScore>();

        assert_eq!(world.snapshot_resources().resource_count(), 0);
    }

    /// A name the loading generation no longer declares is skipped.
    #[test]
    fn a_resource_the_loader_does_not_declare_is_skipped() {
        let mut world = World::new();
        world.register_persistable_resource::<PersistProbeScore>();
        world.insert_resource(PersistProbeScore {
            points: 3,
            label: String::new(),
        });
        let snapshot = world.snapshot_resources();

        // The loading world declares nothing, as a project that dropped the
        // resource type between builds would.
        let mut loaded = World::new();
        loaded.restore_resources(&snapshot);
        assert!(
            loaded.get_resource::<PersistProbeScore>().is_none(),
            "nothing is resurrected for a type this generation does not own"
        );
    }

    /// A field added since the snapshot arrives at the new type's default.
    #[test]
    fn a_field_added_since_the_resource_snapshot_restores_defaulted() {
        let mut world = World::new();
        world.register_persistable_resource_as::<PersistProbeSettingsV1>(PROBE_SETTINGS_NAME);
        world.insert_resource(PersistProbeSettingsV1 { volume: 0.75 });
        let snapshot = world.snapshot_resources();

        // The next build of the same resource, one field wider, registered
        // under the same persistence name.
        let mut loaded = World::new();
        loaded.register_persistable_resource_as::<PersistProbeSettingsV2>(PROBE_SETTINGS_NAME);
        loaded.restore_resources(&snapshot);

        assert_eq!(
            loaded.get_resource::<PersistProbeSettingsV2>(),
            Some(&PersistProbeSettingsV2 {
                volume: 0.75,
                brightness: 0.0,
            }),
            "the carried field survives and the new one starts defined"
        );
    }

    /// A resource declared by another language round-trips as raw bytes.
    #[test]
    fn a_foreign_resource_round_trips_as_bytes() {
        let mut world = World::new();
        let id = world
            .register_foreign_resource("probe::ForeignTuning", "Probe.Tuning", 8, 4, 0x1234)
            .expect("a valid foreign layout registers");
        assert!(world.register_persistable_foreign_resource("probe::ForeignTuning"));
        world
            .insert_foreign_resource_bytes(id, &[1, 2, 3, 4, 5, 6, 7, 8])
            .expect("the payload is the declared width");

        let snapshot = world.snapshot_resources();
        assert_eq!(snapshot.resource_count(), 1);

        let mut loaded = World::new();
        let loaded_id = loaded
            .register_foreign_resource("probe::ForeignTuning", "Probe.Tuning", 8, 4, 0x1234)
            .expect("the loading generation declares the same shape");
        loaded.restore_resources(&snapshot);

        assert_eq!(
            loaded.foreign_resource_bytes(loaded_id),
            Some(&[1u8, 2, 3, 4, 5, 6, 7, 8][..]),
            "every byte survives the round trip"
        );
    }

    /// A foreign payload whose width disagrees with the live declaration is
    /// refused rather than padded.
    #[test]
    fn a_resized_foreign_resource_payload_is_refused() {
        let mut world = World::new();
        let id = world
            .register_foreign_resource("probe::ForeignResized", "Probe.Resized", 8, 4, 1)
            .expect("a valid foreign layout registers");
        assert!(world.register_persistable_foreign_resource("probe::ForeignResized"));
        world
            .insert_foreign_resource_bytes(id, &[9; 8])
            .expect("the payload is the declared width");
        let snapshot = world.snapshot_resources();

        // The loading generation declares the same resource four bytes wider.
        let mut loaded = World::new();
        let loaded_id = loaded
            .register_foreign_resource("probe::ForeignResized", "Probe.Resized", 12, 4, 2)
            .expect("the wider layout is still valid");
        loaded.restore_resources(&snapshot);

        assert_eq!(
            loaded.foreign_resource_bytes(loaded_id),
            None,
            "an unreconcilable payload leaves the resource unset instead of \
             writing a partly-defined value"
        );
    }

    /// A snapshot is byte-reproducible across two identical captures.
    #[test]
    fn a_resource_snapshot_is_ordered_by_name() {
        let mut world = World::new();
        world.register_persistable_resource::<PersistProbeScore>();
        world.register_persistable_resource_as::<PersistProbeSettingsV1>(PROBE_SETTINGS_NAME);
        world.insert_resource(PersistProbeScore::default());
        world.insert_resource(PersistProbeSettingsV1::default());

        let names: Vec<String> = world
            .snapshot_resources()
            .entries
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "entries come out in name order");
    }

    /// A reshaped resource is migrated across a reload rather than
    /// reinterpreted.
    ///
    /// `rehome_resources` swaps a live resource's function table but never its
    /// bytes, so without this pass the arriving generation would read the old
    /// value's memory through the new shape.
    #[test]
    fn a_reshaped_resource_is_migrated_across_a_reload() {
        let mut world = World::new();
        world.register_persistable_resource_as::<PersistProbeSettingsV1>(PROBE_SETTINGS_NAME);
        world.insert_resource(PersistProbeSettingsV1 { volume: 0.5 });

        // What the host captures immediately before the swap.
        let previous_manifest = world.persist_resource_manifest();
        assert_eq!(previous_manifest.len(), 1);

        // The arriving generation registers the wider shape under the same
        // persistence name, which supersedes the outgoing build's entries.
        world.register_persistable_resource_as::<PersistProbeSettingsV2>(PROBE_SETTINGS_NAME);
        let migrated = world.migrate_changed_persistable_resources(&previous_manifest);

        assert_eq!(migrated, vec![PROBE_SETTINGS_NAME.to_string()]);
        assert_eq!(
            world.get_resource::<PersistProbeSettingsV2>(),
            Some(&PersistProbeSettingsV2 {
                volume: 0.5,
                brightness: 0.0,
            }),
            "the value carries across and the added field is defaulted"
        );
    }

    /// An unchanged resource keeps its value untouched across a reload.
    #[test]
    fn an_unchanged_resource_is_not_migrated() {
        let mut world = World::new();
        world.register_persistable_resource_as::<PersistProbeSettingsV1>(PROBE_SETTINGS_NAME);
        world.insert_resource(PersistProbeSettingsV1 { volume: 0.25 });

        let previous_manifest = world.persist_resource_manifest();
        // The same shape re-registers, as an unedited reload does.
        world.register_persistable_resource_as::<PersistProbeSettingsV1>(PROBE_SETTINGS_NAME);

        assert!(
            world
                .migrate_changed_persistable_resources(&previous_manifest)
                .is_empty(),
            "an unchanged schema takes the fast path"
        );
        assert_eq!(
            world.get_resource::<PersistProbeSettingsV1>(),
            Some(&PersistProbeSettingsV1 { volume: 0.25 }),
            "and the value is left exactly as it was"
        );
    }

    /// Changing only a default *value* is not a schema change.
    ///
    /// The hash normalizes values to kind markers, so editing a literal in a
    /// `Default` impl does not cost every player their stored settings.
    #[test]
    fn a_changed_default_value_does_not_count_as_a_reshape() {
        let mut world = World::new();
        world.register_persistable_resource_as::<PersistProbeSettingsV1>(PROBE_SETTINGS_NAME);
        world.insert_resource(PersistProbeSettingsV1 { volume: 0.9 });
        let first = world.persist_resource_manifest();

        let mut other = World::new();
        other.register_persistable_resource_as::<PersistProbeSettingsV1>(PROBE_SETTINGS_NAME);
        let second = other.persist_resource_manifest();

        assert_eq!(first[0].schema_hash, second[0].schema_hash);
    }
}
