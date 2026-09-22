//! Central ECS state container - entities, archetypes, components, and resources.
//!
//! # Responsibilities
//!
//! - Declares the [`World`] struct itself: every map and counter the engine's
//!   state consists of, and the one place they are constructed.
//! - Manages entity creation, destruction, and ID recycling via a free list.
//! - Owns all archetypes, including the migration that moves one entity's row
//!   from one archetype to another.
//!
//! # Design
//!
//! The [`World`] is the central hub of the ECS. It allocates entity IDs,
//! manages archetype storage, tracks entity-to-archetype mappings, and stores
//! resources (singleton data). Component types must be registered before use
//! so the world can assign bit indices for archetype mask matching. Entity
//! destruction recycles IDs through a free list with generation counters
//! to prevent dangling-handle bugs.
//!
//! One type, several jobs, one `impl` per job. `World` is what every other part
//! of the engine reaches through, so its surface is the engine's effective API,
//! and a single block holding all of it is more than a reader can keep in view.
//! The jobs are genuinely separable - script dispatch has nothing to do with
//! resource claims, which have nothing to do with field reflection - so each
//! gets a file of the module tree below, and the directory listing says what
//! they are instead of section banners inside one file. Nothing about the type
//! or its API changes: an `impl World` block in a child module is the same
//! `impl World`, and a child module reaches the struct's private fields, so the
//! split widens no visibility.

// Standard library
use std::collections::HashMap;

// External crates
use pill_core::{error, warn};

// Current crate
use crate::archetype::{
    validate_component_layout, Archetype, ArchetypeId, Blittability, ColumnLayout,
    ComponentColumns, FieldPlan, FieldSource, StorageFactory,
};
use crate::commands::CommandQueue;
use crate::component::{
    Component, ComponentId, ComponentMask, ComponentRegistry, ComponentTicks, Tick,
};
use crate::entity::Entity;
use crate::query::change_detection::Mut;
use crate::resource::{ErasedResource, ErasedResourceOps, Resource, ResourceId};
use crate::scripting::{ScriptComponent, ScriptContext};

// =============================================================================
// Module Tree
// =============================================================================
//
// `World` is one type with several jobs, so its `impl` is split across these
// files by responsibility rather than kept in one block. Every file is part of
// the same module tree, which is what lets them reach the struct's private
// fields without any of those fields being widened for the split.

mod components;
mod reflection;
mod resources;
mod scripting;
mod ticks;

// =============================================================================
// Re-exports
// =============================================================================

pub use crate::error::{AddComponentError, BuildError, RemoveComponentError, WorldError};

// =============================================================================
// Registration headroom
// =============================================================================

/// Component-type headroom below which registration warns.
///
/// The 128-type ceiling is shared across a project and every extension,
/// so it can be exhausted even with few of "your own" types. Warn while there
/// is still room to act, so exhaustion is visible before it is fatal.
const REGISTRATION_HEADROOM_WARNING_THRESHOLD: usize = 16;

// =============================================================================
// Per-Thread Last-Run Tick (thread-local statics)
// =============================================================================
//
// In parallel mode, multiple systems run on different threads simultaneously.
// Each system needs its own "last ran at tick X" value so change-detection
// filters compare against the correct baseline.  A single shared field on
// World would race, so we use thread-local storage instead.
//
// How it works:
//   1. Before running system A on thread 1:  store tick 5 in thread-local
//   2. Before running system B on thread 2:  store tick 3 in thread-local
//   3. Inside each system, world.system_last_run() checks the thread-local
//      first → each thread sees its own value, no sharing, no race.
//   4. After the system finishes: restore the previous value (usually None).
//
// The same storage reserves each parallel system's "this run" tick. A batch
// runs every system against the same world, so a bump there would be an
// unsynchronized read-modify-write; instead the engine allocates one tick per
// batch member on the dispatch thread and installs it here, and
// `increment_change_tick` serves it without touching the shared counter.
//
// In sequential mode both overrides stay None, so queries fall back to the
// shared world fields - no thread-local overhead.

thread_local! {
    /// Each thread's private "my system last ran at tick ___" value.
    /// `None` means "not in a parallel batch - use the world field."
    static PER_THREAD_LAST_RUN_TICK: std::cell::Cell<Option<Tick>> =
        const { std::cell::Cell::new(None) };

    /// The tick reserved for the system running on this thread.
    /// `None` means "bump the shared counter"; `Some` is the one value the
    /// engine allocated for this batch member before dispatch.
    static PER_THREAD_THIS_RUN_TICK: std::cell::Cell<Option<Tick>> =
        const { std::cell::Cell::new(None) };
}

#[inline]
fn per_thread_last_run_tick() -> Option<Tick> {
    PER_THREAD_LAST_RUN_TICK.with(|cell| cell.get())
}

#[inline]
fn per_thread_this_run_tick() -> Option<Tick> {
    PER_THREAD_THIS_RUN_TICK.with(|cell| cell.get())
}

/// Whether a size and alignment can describe a foreign resource's allocation.
///
/// The predicate both [`World::register_foreign_resource`] and
/// [`World::relayout_foreign_resource`] need, kept in one place so the two
/// cannot drift apart: a declaration that passed while its next relayout did
/// not would leave the world holding a shape it can no longer store.
fn is_valid_foreign_layout(size: usize, align: usize) -> bool {
    size != 0
        && align != 0
        && align.is_power_of_two()
        && std::alloc::Layout::from_size_align(size, align).is_ok()
}

/// Store a per-thread baseline tick, returning the old value so the
/// caller can restore it when the system finishes (RAII-style).
#[inline]
pub(crate) fn set_per_thread_last_run_tick(value: Option<Tick>) -> Option<Tick> {
    PER_THREAD_LAST_RUN_TICK.with(|cell| cell.replace(value))
}

/// Store the tick reserved for the system about to run on this thread,
/// returning the old value so the caller can restore it afterwards.
#[inline]
pub(crate) fn set_per_thread_this_run_tick(value: Option<Tick>) -> Option<Tick> {
    PER_THREAD_THIS_RUN_TICK.with(|cell| cell.replace(value))
}

// =============================================================================
// EntityLocation
// =============================================================================

/// Tracks where an entity is stored in the archetype system.
///
/// Maps an [`Entity`] handle to the [`Archetype`] that owns its component
/// columns plus the row index within that archetype. Updated on every entity
/// creation, migration, and destruction so that random-access component
/// lookups stay O(1).
#[derive(Clone, Copy)]
pub(crate) struct EntityLocation {
    /// The archetype that currently stores the entity's components.
    pub(crate) archetype_id: ArchetypeId,
    /// Row of the entity inside its archetype's parallel columns.
    pub(crate) index_in_archetype: usize,
}

// =============================================================================
// IteratorTimings
// =============================================================================

/// Shared state for per-label iterator timing feedback.
pub(crate) struct IteratorTimings {
    /// Per-label splitting hint duration (ns), ~32-frame average.
    pub per_iterator_label_average_duration: std::collections::HashMap<&'static str, u64>,
    /// Labels visited in the current frame. Cleared each frame.
    pub visited_iterator_labels: Vec<&'static str>,
    /// Labels that appeared more than once in the current frame.
    pub visited_duplicated_iterator_labels: Vec<&'static str>,
}

impl IteratorTimings {
    /// Creates an empty [`IteratorTimings`] with no labels recorded.
    pub fn new() -> Self {
        Self {
            per_iterator_label_average_duration: std::collections::HashMap::new(),
            visited_iterator_labels: Vec::new(),
            visited_duplicated_iterator_labels: Vec::new(),
        }
    }
}

/// What a shared resource name was claimed with.
///
/// Recorded per shared [`ResourceId`] so a second claim can be checked against
/// the first: the same type arriving from another artifact is the case the
/// feature exists for, while a different type claiming one name is a collision.
#[derive(Debug, Clone)]
pub struct SharedResourceClaim {
    /// The shared name the claiming type declared.
    pub shared_name: String,
    /// Name of the first declarer: a Rust type's final path segment, or a
    /// managed full name for a declaration from another language.
    pub declaring_type: String,
    /// Whether the first declarer was another language's declaration.
    ///
    /// Such a claim carries no Rust type name to compare against, so the name
    /// check applies only between two Rust declarations - see
    /// [`World::claim_shared_resource`].
    pub foreign: bool,
    /// Size in bytes the claiming type declared.
    pub size: usize,
    /// Alignment in bytes the claiming type declared.
    pub align: usize,
    /// Structural hash of the claiming type's layout, when one is available.
    ///
    /// Foreign declarations carry theirs from the manifest; Rust declarations
    /// carry whatever [`Resource::shared_schema_hash`] answers, which defaults
    /// to `None`. Compared only when both sides have one, so the check is
    /// extra evidence rather than a new requirement.
    pub schema_hash: Option<u64>,
}

// =============================================================================
// World
// =============================================================================

/// One live entity and the type names of the components attached to it.
///
/// Returned by [`World::entity_rows`] for the editor's Hierarchy panel. The
/// names are stable across hot reloads; the [`Entity`] handle is the stable
/// selection identity.
#[derive(Clone, Debug, PartialEq)]
pub struct EntityRow {
    /// The live entity handle, generation-tagged.
    pub entity: Entity,
    /// Registered type names of the components attached to the entity.
    pub components: Vec<String>,
}

/// Where a component's field layout came from.
///
/// One component's field list, declared either way.
///
/// Native components submit a `&'static` slice from their declaring
/// artifact's static data; components defined by another language describe
/// themselves in a runtime manifest, so their layout is owned by the `World`
/// instead. The difference is storage, not meaning, so it lives in a single
/// `Cow`; the editor reads the list through [`World::component_field_layout`].
#[derive(Debug, Clone)]
pub(crate) struct ComponentFieldLayout(
    std::borrow::Cow<'static, [crate::component_registry::ComponentFieldDescriptor]>,
);

impl ComponentFieldLayout {
    /// Wrap a compile-time layout living in the declaring artifact's static data.
    pub(crate) fn from_static(
        fields: &'static [crate::component_registry::ComponentFieldDescriptor],
    ) -> Self {
        Self(std::borrow::Cow::Borrowed(fields))
    }

    /// Wrap a runtime-described layout owned by the world (foreign-language
    /// components).
    pub(crate) fn from_owned(
        fields: Vec<crate::component_registry::ComponentFieldDescriptor>,
    ) -> Self {
        Self(std::borrow::Cow::Owned(fields))
    }

    /// The field list, whichever lane declared it.
    pub(crate) fn fields(&self) -> &[crate::component_registry::ComponentFieldDescriptor] {
        &self.0
    }
}

/// Manages all entities, archetypes, and resources in the ECS.
///
/// The central hub of the engine. It allocates entity IDs with generation
/// counters, owns the archetype storage for every component combination,
/// tracks entity-to-archetype locations, stores singleton resources, and
/// maintains the component type registry used to build archetype storage.
pub struct World {
    /// Next fresh entity ID handed out when the free list is empty.
    next_free_entity_id: u64,
    /// Free list of recycled entity IDs with their next generation. Stored as (id, next_generation) pairs
    pub(crate) free_entity_ids: Vec<(u64, u32)>,
    /// All archetypes in the world
    pub(crate) archetypes: HashMap<ArchetypeId, Archetype>,
    /// Tracks where each entity is located in the archetype system
    pub(crate) entity_locations: HashMap<Entity, EntityLocation>,
    /// Storage factories for creating component storage by TypeId
    pub(crate) storage_factories: HashMap<ComponentId, StorageFactory>,
    /// The native function table each id held before its latest registration
    /// replaced it.
    ///
    /// The in-place migration consumes columns that still hold the *old*
    /// layout's values, so it has to release them through the glue that wrote
    /// them; by the time it runs, `rehome_native_columns` has already stamped
    /// the arriving generation's table onto every column.
    /// `register_component_inner` records the outgoing table here so the
    /// migration can put it back for the removal that drops those rows.
    pub(crate) retired_native_storage_ops: HashMap<ComponentId, crate::archetype::ColumnOps>,
    /// Script component types (ComponentId, component mask bit)
    script_components: Vec<(ComponentId, u8)>,
    /// Script updaters for calling update() on script components
    script_updaters: HashMap<ComponentId, scripting::ScriptUpdater>,
    /// Component registry for bit indices and names
    pub(crate) component_registry: ComponentRegistry,
    /// Resources (singleton data) stored by type.
    ///
    /// [`ErasedResource`] rather than `Box<dyn Any>`: a trait object carries
    /// its destructor in a vtable belonging to the artifact that created the
    /// value, and resources are never cleared on reload, so retiring a module
    /// that owned one would leave that destructor pointing into an unmapped
    /// image. The erased box holds a replaceable function table instead - the
    /// same discipline component columns already use.
    pub(crate) resources: HashMap<ResourceId, ErasedResource>,
    /// Per-resource change-detection ticks (parallel to `resources`).
    pub(crate) resource_ticks: HashMap<ResourceId, ComponentTicks>,
    /// Latest per-type function table registered for each resource id.
    ///
    /// The counterpart of `storage_factories` for components, and what
    /// [`Self::rehome_resources`] refreshes live resources from. Written by
    /// [`Self::insert_resource`] and [`Self::register_resource`], so a
    /// generation that re-registers a type without re-inserting its value
    /// still contributes a table pointing at code that is mapped.
    pub(crate) resource_factories: HashMap<ResourceId, ErasedResourceOps>,
    /// Declared field layout of each foreign resource, when its declarer sent
    /// one.
    ///
    /// The resource twin of `component_field_layouts`, and kept for the same
    /// reason: a foreign resource's bytes are opaque without it, so the editor
    /// and any reader that wants named fields has nothing to go on. It is
    /// separate from `resource_factories` because a layout is optional - a
    /// resource registered without one still works, it just is not
    /// inspectable.
    pub(crate) resource_field_layouts: HashMap<ResourceId, ComponentFieldLayout>,
    /// For each shared resource id, the Rust type that claimed it and the
    /// layout it declared.
    ///
    /// Only shared ids appear here: an ordinary resource is identified by its
    /// `TypeId`, so a second claim on the same id is by definition the same
    /// type. Mirrors what `ComponentRegistry` records for shared components.
    pub(crate) shared_resource_claims: HashMap<ResourceId, SharedResourceClaim>,
    /// Monotonic counter stamped onto each resource registration.
    pub(crate) resource_registration_sequence: u64,
    /// First registration sequence stamped for each resource id.
    ///
    /// The resource twin of the component registration log, and needed for the
    /// same reason: `resource_factories` accumulates and never forgets, so it
    /// cannot distinguish "registered by the generation now running" from
    /// "registered by a generation that has since been retired". One entry per
    /// id, not per registration: `insert_resource` documents itself as the
    /// *replace* path, and stamping every replacement grew this for the
    /// process lifetime while each reload diff scanned and sorted it.
    pub(crate) resource_registration_stamps: HashMap<ResourceId, u64>,
    /// How many subjects currently claim each resource id.
    ///
    /// A shared id can be registered *by name* from more than one subject -
    /// the project and a module, say - and `drop_resources` is how one of them
    /// retires what its generation stopped registering. Without this count the
    /// first subject to retire the id destroys a value the other still uses,
    /// claim and all. Ids with no entry are dropped unconditionally, exactly
    /// as before.
    pub(crate) resource_claim_counts: HashMap<ResourceId, u32>,
    /// Monotonically increasing world tick used for change detection.
    ///
    /// Bumped once per frame by the [`Engine`](crate::engine::Engine) and
    /// also each time a query that supports change tracking begins iteration.
    /// Stored as a plain `u32`; wrap-around handling is intentionally simple
    /// (and matches the expected lifetime of long-running games at 60 fps:
    /// ~828 days before overflow).
    pub(crate) change_tick: u32,
    /// Last-run tick for the system that is currently fetching its
    /// parameters from this world.
    ///
    /// The [`Engine`](crate::engine::Engine) sets this immediately before
    /// invoking each system so that change-detection filters
    /// (e.g. `Changed<T>`, `Added<T>`) constructed inside the system
    /// compare against the correct baseline. Defaults to `0`, meaning
    /// "since the beginning of time" - useful for ad-hoc queries that are
    /// not driven by the engine.
    pub(crate) system_last_run: u32,

    /// Monotonically incrementing generation counter. Bumped whenever
    /// an archetype is added or removed.  Queries use this to cache
    /// matching archetype lists - if the generation hasn't changed,
    /// the cached list is still valid. Solely optimization reasons.
    pub(crate) archetype_generation: u64,

    /// Debug-only: Tracks which resources currently have an active
    /// mutable borrow.  Used to catch scheduler bugs where two systems
    /// obtain `&mut` to the same resource simultaneously.
    ///
    /// Cleared at the start of every frame by the Engine.
    ///
    /// Behind a mutex because the systems of one parallel batch each rebuild
    /// `&mut World` from the same pointer and run concurrently, so two of them
    /// taking `ResMut` of *different* resources would otherwise mutate this set
    /// from two threads at once. Their resource ids are disjoint - the
    /// scheduler guarantees that - but a `HashSet` is not safe to mutate
    /// concurrently whatever the keys are.
    #[cfg(debug_assertions)]
    pub(crate) debug_resource_write_locks:
        parking_lot::Mutex<std::collections::HashSet<ResourceId>>,

    /// Number of deferred commands executed in the current frame.
    /// Set by `CommandQueue::execute_queued_commands`, read by the Engine for Tracy plots.
    pub(crate) commands_executed_this_frame: usize,

    /// Per-label splitting hint execution timing for parallel iterators.
    /// Keyed by the `label()` string set on each `ParQueryIter`.
    /// Shared via `Arc` so iterators can read/write without a raw
    /// World pointer - the `Mutex` handles concurrent access from
    /// systems in the same engine batch.
    pub(crate) iterator_timings: std::sync::Arc<std::sync::Mutex<IteratorTimings>>,

    /// Per-component-type serialize fn for snapshotting (persistence module).
    pub(crate) persist_serializers: HashMap<ComponentId, crate::persistence::SerializeComponentFn>,
    /// Per-type-name deserialize fn for restoring (persistence module).
    pub(crate) persist_deserializers: HashMap<String, crate::persistence::DeserializeComponentFn>,
    /// Per-component-type insert fn for pushing Box<dyn Component> into storage.
    pub(crate) persist_inserters: HashMap<ComponentId, crate::persistence::InsertComponentFn>,
    /// Per-type-name schema hash for persistable components.
    pub(crate) persist_schema_hashes: HashMap<String, u64>,
    /// Monotonic counter bumped on every persistable registration, letting the
    /// host enumerate exactly which types one module's `init` registered.
    pub(crate) persist_registration_sequence: u64,
    /// Chronological `(type_name, sequence)` log of persistable registrations.
    pub(crate) persist_registration_log: Vec<(String, u64)>,
    /// Type names the host announced as superseded for the init pass that is
    /// about to run, so a reloaded generation may replace its predecessor's
    /// persist entries instead of being refused as a concurrent peer.
    pub(crate) superseded_persist_names: std::collections::HashSet<String>,

    /// Per-resource serialize fn for snapshotting, keyed by the live id.
    ///
    /// Keyed by id rather than name because the serializer has to find the
    /// value, and a value is stored under its id. Its twin below is keyed by
    /// name because a restore starts from a snapshot entry, which carries a
    /// name and never an id.
    pub(crate) persist_resource_serializers:
        HashMap<ResourceId, crate::persistence::SerializeResourceFn>,
    /// Per-name restore fn for rebuilding a resource out of snapshot bytes.
    pub(crate) persist_resource_restorers: HashMap<String, crate::persistence::RestoreResourceFn>,
    /// The persistence name each registered resource id was recorded under.
    pub(crate) persist_resource_names: HashMap<ResourceId, String>,
    /// Per-name schema hash for persistable resources, so a reload can tell a
    /// reshaped resource from an unchanged one.
    pub(crate) persist_resource_schema_hashes: HashMap<String, u64>,
    /// Monotonic counter bumped on every component registration (plain or
    /// persistable), letting the host enumerate which types one module's
    /// `init` registered at all — the distinction between a type that was
    /// dropped entirely and one merely downgraded to a plain component.
    pub(crate) component_registration_sequence: u64,
    /// Chronological `(type_name, sequence)` log of every component
    /// registration, plain and persistable alike.
    pub(crate) component_registration_log: Vec<(String, ComponentId, u64)>,
    /// Field layouts submitted by `#[derive(PillComponent)]` (static, living
    /// in the declaring artifact) or described at runtime by a foreign-language
    /// manifest (owned). Consumed by the C# mirror codegen and the editor's
    /// generic inspector. Re-registered by each reloaded generation; a
    /// descriptor manifest replaces rather than accumulates.
    pub(crate) component_field_layouts: HashMap<ComponentId, ComponentFieldLayout>,
    /// First component-registration failure of the current init pass, if any.
    ///
    /// Set when the 128-type ceiling is hit (or any other registry error
    /// surfaces during `register_component`). The artifact-wide registration
    /// loop drains it so the generated `init` can fail the reload
    /// transactionally instead of running with a half-registered component set.
    registration_error: Option<WorldError>,
}

impl World {
    /// Create a new empty World
    pub fn new() -> Self {
        Self {
            next_free_entity_id: 0,
            free_entity_ids: Vec::new(),
            archetypes: HashMap::new(),
            entity_locations: HashMap::new(),
            storage_factories: HashMap::new(),
            retired_native_storage_ops: HashMap::new(),
            script_components: Vec::new(),
            script_updaters: HashMap::new(),
            component_registry: ComponentRegistry::new(),
            resources: HashMap::new(),
            resource_ticks: HashMap::new(),
            resource_factories: HashMap::new(),
            resource_field_layouts: HashMap::new(),
            shared_resource_claims: HashMap::new(),
            resource_registration_sequence: 0,
            resource_registration_stamps: HashMap::new(),
            resource_claim_counts: HashMap::new(),
            change_tick: 0,
            system_last_run: 0,
            archetype_generation: 0,
            #[cfg(debug_assertions)]
            debug_resource_write_locks: parking_lot::Mutex::new(std::collections::HashSet::new()),
            commands_executed_this_frame: 0,
            iterator_timings: std::sync::Arc::new(std::sync::Mutex::new(IteratorTimings::new())),
            persist_serializers: HashMap::new(),
            persist_deserializers: HashMap::new(),
            persist_inserters: HashMap::new(),
            persist_schema_hashes: HashMap::new(),
            persist_registration_sequence: 0,
            persist_registration_log: Vec::new(),
            superseded_persist_names: std::collections::HashSet::new(),
            persist_resource_serializers: HashMap::new(),
            persist_resource_restorers: HashMap::new(),
            persist_resource_names: HashMap::new(),
            persist_resource_schema_hashes: HashMap::new(),
            component_registration_sequence: 0,
            component_registration_log: Vec::new(),
            component_field_layouts: HashMap::new(),
            registration_error: None,
        }
    }

    /// Reserve capacity for at least `additional` more entities.
    ///
    /// Pre-allocates internal data structures to avoid reallocation
    /// overhead when creating many entities in a batch. Call this
    /// before a loop that calls `create_entity()` for best performance.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use pill_engine::*;
    /// # let mut world = World::new();
    /// world.reserve_entities(10_000);
    /// for _ in 0..10_000 {
    ///     world.create_entity(); // No reallocation overhead
    /// }
    /// ```
    pub fn reserve_entities(&mut self, additional: usize) {
        let _zone = crate::profile_scope!(
            "reserve entities",
            [("Additional entities to reserve: {}", additional)]
        );
        self.free_entity_ids.reserve(additional);
        self.entity_locations.reserve(additional);
    }

    /// Return one archetype-sized entity chunk for language bindings.
    ///
    /// Entity chunks enumerate every archetype and provide the driver for
    /// `EntityTerm` and queries containing only optional component terms.
    pub fn entity_chunk(&self, chunk_index: usize) -> Option<(ArchetypeId, &[Entity])> {
        let archetype = self.archetypes.values().nth(chunk_index)?;
        Some((archetype.id, archetype.entities.as_slice()))
    }

    /// Return one already-known archetype's entity column.
    ///
    /// The archetype-scoped twin of [`Self::entity_chunk`], used by language
    /// bindings that identified the archetype through a driver chunk.
    pub fn entity_chunk_in_archetype(
        &self,
        archetype_id: ArchetypeId,
    ) -> Option<(ArchetypeId, &[Entity])> {
        let archetype = self.archetypes.get(&archetype_id)?;
        Some((archetype.id, archetype.entities.as_slice()))
    }

    /// Entity IDs that were destroyed and are waiting to be handed out again.
    ///
    /// The engine's own retirement pool: an ID here belongs to no live entity,
    /// but the slot is kept so a later spawn reuses it with a bumped
    /// generation rather than growing the ID space.
    pub fn recycled_entity_id_count(&self) -> usize {
        self.free_entity_ids.len()
    }

    /// Create an entity consisting entirely of runtime-defined components.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DescriptorEntityEmpty`] if `components` is empty,
    /// [`WorldError::DescriptorDuplicateComponent`] if a component appears more
    /// than once, [`WorldError::DescriptorComponentNotRegistered`] if a
    /// component was never registered, and
    /// [`WorldError::DescriptorByteLengthMismatch`] if a byte payload does not
    /// match its registered layout size.
    pub fn create_descriptor_entity(
        &mut self,
        components: &[(ComponentId, Vec<u8>)],
    ) -> Result<Entity, WorldError> {
        // Step 1: Validate the component set - non-empty, unique, registered,
        // and every payload matches its registered layout.
        if components.is_empty() {
            return Err(WorldError::DescriptorEntityEmpty);
        }
        let mut component_ids: Vec<_> = components.iter().map(|(id, _)| *id).collect();
        component_ids.sort();
        component_ids.dedup();
        if component_ids.len() != components.len() {
            return Err(WorldError::DescriptorDuplicateComponent);
        }
        for (id, bytes) in components {
            let Some(StorageFactory::Descriptor(layout)) = self.storage_factories.get(id) else {
                return Err(WorldError::DescriptorComponentNotRegistered { id: *id });
            };
            if bytes.len() != layout.size {
                return Err(WorldError::DescriptorByteLengthMismatch { id: *id });
            }
        }

        // Step 2: Allocate an entity handle and get or create the archetype
        // for this component set.
        let entity = self.allocate_entity();
        let archetype_id = self.get_or_create_archetype(component_ids);
        let current_tick = Tick::new(self.change_tick);
        let Some(archetype) = self.archetypes.get_mut(&archetype_id) else {
            return Err(WorldError::ArchetypeMissing {
                entity,
                archetype_id,
            });
        };

        // Step 3: Confirm every requested column exists before touching any of
        // them. A manifest that registers a component without creating its
        // storage would otherwise leave a half-populated row behind, so this
        // pre-flight pass keeps the failure atomic.
        for (id, _) in components {
            if !archetype.component_storages.contains(*id) {
                return Err(WorldError::DescriptorStorageMissing {
                    component_id: *id,
                    archetype_id,
                });
            }
        }

        let index = archetype.entities.len();
        archetype.entities.push(entity);

        // Step 4: Push each raw byte payload into the entity's new row and
        // stamp its arrival tick. The push grows the column's ticks with it, so
        // there is one lookup rather than two; it cannot fail after the pass
        // above, but it reports rather than panics so a future refactor that
        // drops the pre-flight check degrades into an error instead of
        // unwinding.
        for (id, bytes) in components {
            match archetype.component_storages.get_mut(*id) {
                Some(storage) => {
                    storage.push_bytes(bytes)?;
                    storage.set_row_ticks(index, ComponentTicks::new(current_tick));
                }
                None => {
                    return Err(WorldError::DescriptorStorageMissing {
                        component_id: *id,
                        archetype_id,
                    })
                }
            }
        }

        // Step 5: Record where the entity lives so random access stays O(1).
        self.entity_locations.insert(
            entity,
            EntityLocation {
                archetype_id,
                index_in_archetype: index,
            },
        );
        Ok(entity)
    }

    /// The type-independent half of [`Self::claim_shared_resource_name`].
    ///
    /// A declaration from another language has no Rust type to ask, so it hands
    /// over the same four facts its manifest carries. `foreign` decides one
    /// thing: whether the declaring names are compared. Two Rust types reaching
    /// one shared name are a collision, and their own names are what tells two
    /// copies of one type from two different types. A foreign declaration has
    /// no Rust name to compare against a final path segment, and writing the
    /// shared name down *is* its statement of which resource it means - so
    /// there the layout is the check.
    ///
    /// The recorded claim deliberately keeps its Rust declarer when a foreign
    /// declaration joins: that record is what a later Rust arrival is compared
    /// against, and a managed name would make every later arrival look
    /// different. A Rust arrival to a foreign claim does take the record, for
    /// the same reason in reverse.
    ///
    /// Returns the error, and does not record it: the Rust wrapper above
    /// records what its drain has to see, while a foreign declaration's caller
    /// is the one that must act on the refusal.
    // Seven facts about one declaration, each of them read from the manifest
    // or the Rust type rather than derived: grouping them into a struct would
    // hide that every field is a separate claim about the same resource.
    /// Check if an entity exists and is valid (not destroyed/recycled)
    ///
    /// Returns true if the entity exists in the world with the correct generation.
    /// Returns false if the entity was destroyed or if its ID was recycled with a new generation.
    #[must_use]
    pub fn is_entity_valid(&self, entity: Entity) -> bool {
        self.entity_locations.contains_key(&entity)
    }

    /// Allocate a new unique entity ID
    ///
    /// Reuses IDs from the free list when available, incrementing the generation
    /// to invalidate any stale handles. Otherwise allocates a fresh ID.
    pub(crate) fn allocate_entity(&mut self) -> Entity {
        let _zone = crate::profile_scope!(
            "allocate entity",
            [
                (
                    "Free entity IDs available for reuse: {}",
                    self.free_entity_ids.len()
                ),
                (
                    "Next fresh entity ID to allocate: {}",
                    self.next_free_entity_id
                )
            ]
        );
        // Try to reuse an ID from the free list
        if let Some((id, generation)) = self.free_entity_ids.pop() {
            Entity { id, generation }
        } else {
            // Allocate a fresh ID
            let entity = Entity {
                id: self.next_free_entity_id,
                generation: 0,
            };
            self.next_free_entity_id += 1;
            entity
        }
    }

    /// Reserve a generation-checked handle for a deferred entity creation.
    ///
    /// The returned entity is not visible to queries until a command inserts
    /// it. This is the type-erased counterpart of [`Commands::create_entity`]
    /// used by foreign-language runtimes.
    pub fn reserve_entity(&mut self) -> Entity {
        self.allocate_entity()
    }

    /// Return a reserved entity handle that was never created.
    ///
    /// The handle must have come from [`Self::reserve_entity`] and must not
    /// have been inserted into any archetype. Releasing an entity that is
    /// visible to queries would allow its id to be handed out again with a
    /// conflicting generation.
    pub fn release_entity(&mut self, entity: Entity) {
        self.free_entity_ids.push((entity.id, entity.generation));
    }

    /// Get or create an archetype for a given set of components
    ///
    /// Archetypes are cached and reused for entities with the same component set.
    /// The lookup uses ComponentMask for O(1) hash lookup, avoiding repeated sorting.
    pub(crate) fn get_or_create_archetype(
        &mut self,
        component_ids: Vec<ComponentId>,
    ) -> ArchetypeId {
        let _zone = crate::profile_scope!(
            "get or create archetype",
            [("Component types in archetype: {}", component_ids.len())]
        );
        // Build component mask first - this is used for the fast lookup path
        // The mask uniquely identifies the component set regardless of order
        let mut component_mask = ComponentMask::empty();
        for component_id in &component_ids {
            if let Some(bit) = self.component_registry.get_bit(component_id) {
                component_mask.set(bit);
            }
        }

        // Derive the archetype ID directly from the mask - the mask uniquely
        // identifies the component set, so no separate lookup table is needed.
        let archetype_id = ArchetypeId(component_mask.bits());

        // Hot path: archetype already exists (most common case).
        //
        // A hit is trusted on the strength of the bit-release rule: a bit only
        // returns to the pool once every archetype carrying it has been
        // dropped, so the cached component set cannot be a different one. The
        // assertion holds that invariant here if the release path is ever
        // bypassed - the mask *is* the id, so an alias would serve the wrong
        // columns with no other symptom.
        if let Some(existing) = self.archetypes.get(&archetype_id) {
            debug_assert!(
                existing.component_types.len() == component_ids.len()
                    && component_ids
                        .iter()
                        .all(|component_id| existing.component_types.contains(component_id)),
                "a recycled component bit aliased two component sets onto one archetype id"
            );
            return archetype_id;
        }

        // Cold path: create new archetype (only sort when actually creating)
        let mut sorted_ids = component_ids;
        sorted_ids.sort();

        crate::profile_message!(
            "new archetype created: {:?} with {} component types (total archetypes now: {})",
            ArchetypeId(component_mask.bits()),
            sorted_ids.len(),
            self.archetypes.len() + 1,
        );

        // Create archetype with storage for all component types
        let new_archetype = Archetype::new(
            archetype_id,
            sorted_ids,
            component_mask,
            &self.storage_factories,
        );
        self.archetypes.insert(archetype_id, new_archetype);
        self.archetype_generation = self.archetype_generation.wrapping_add(1);

        archetype_id
    }

    /// Start building a new entity
    ///
    /// Returns an EntityBuilder that allows fluent API for adding components.
    pub fn create_entity(&'_ mut self) -> EntityBuilder<'_> {
        let entity = self.allocate_entity();
        EntityBuilder {
            world: self,
            entity,
            // Most entities have 3-8 components; pre-allocate to avoid
            // reallocation during .with() chains.
            components: Vec::with_capacity(
                crate::config::EntityBuilderConfig::DEFAULT_COMPONENTS_CAPACITY,
            ),
        }
    }

    /// Insert an entity with its components into the appropriate archetype
    ///
    /// Note: the archetype's columns are type-erased, so pushing a component
    /// still needs its concrete type. Components are added via EntityBuilder,
    /// which has access to those types.
    pub(crate) fn insert_entity_with_components<F>(
        &mut self,
        entity: Entity,
        component_ids: Vec<ComponentId>,
        insert_fn: F,
    ) where
        F: FnOnce(&mut ComponentColumns),
    {
        let _zone = crate::profile_scope!(
            "insert entity",
            [("Target entity being inserted: {:?}", entity)]
        );
        // Step 1: Get or create the archetype for this component set.
        let archetype_id = self.get_or_create_archetype(component_ids);
        let current_tick = Tick::new(self.change_tick);

        let archetype = self.archetypes.get_mut(&archetype_id).unwrap_or_else(|| {
            // `get_or_create_archetype` either returns an existing entry
            // or inserts a fresh one, so a miss here is an
            // internal-invariant break. Naming the archetype and entity
            // makes the report useful.
            panic!(
                "archetype {archetype_id:?} vanished after get_or_create_archetype \
                     while inserting entity {entity:?}"
            )
        });
        let index: usize = archetype.entities.len();

        // Step 2: Append the entity row, then let the closure push each
        // component's concrete value into its column.
        archetype.entities.push(entity);

        // Use the provided closure to insert components with their concrete types
        insert_fn(&mut archetype.component_storages);

        // Step 3: Type-erased components have no concrete Rust value for
        // `insert_fn` to push. Allocate their rows here; the command executor
        // overwrites the zero bytes before the new entity becomes observable.
        for &component_id in &archetype.component_types {
            // `insert_fn` above pushed a row for every component that has a
            // Rust type. Only descriptor columns are still empty.
            if component_id.is_native_storage() {
                continue;
            }
            if let Some(column) = archetype.component_storages.get_mut(component_id) {
                // The layout reached storage only after validation, so this
                // can fail only after pushing roughly 2^60 rows of one
                // component: out of address space rather than out of layout.
                // The fallible paths - `move_entity_to_archetype` and
                // `relayout_descriptor_component` - hand the error back instead;
                // this one returns nothing, so it names the one state it
                // cannot survive.
                column
                    .push_zeroed()
                    .expect("descriptor column growth cannot exhaust the address space");
            }
        }

        // Step 4: Stamp the arrival tick on the row every column just grew by.
        // A column carries its own ticks, so the row is already there; what it
        // cannot know is which tick the world is on.
        for &component_id in &archetype.component_types {
            if let Some(column) = archetype.component_storages.get_mut(component_id) {
                column.set_row_ticks(index, ComponentTicks::new(current_tick));
            }
        }

        // Step 5: Record where the entity lives so random access stays O(1).
        self.entity_locations.insert(
            entity,
            EntityLocation {
                archetype_id,
                index_in_archetype: index,
            },
        );
    }

    /// Move an entity to a new archetype, carrying every component it keeps.
    ///
    /// Every column the two archetypes share is moved bitwise: the row's bytes
    /// travel to the destination and the source releases them without running
    /// drop glue, because ownership went with the bytes. There is one lane for
    /// every component, native or descriptor - a component holding a `Vec` pays
    /// a pointer copy rather than a deep clone followed by a deep drop, and no
    /// `Clone` impl runs during what the engine calls a relocation.
    ///
    /// `attach` fills the columns the destination has and the source does not,
    /// which is exactly the component being added, or nothing for a removal. It
    /// runs before the carried rows move, because that is the only part of the
    /// migration that can still fail: refusing there costs nothing but the
    /// entity's place in the destination's list, while refusing afterwards
    /// would leave rows in two archetypes at once.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::EntityNotFound`] when the entity has no location
    /// record, [`WorldError::ArchetypeMissing`] when either the source or the
    /// destination archetype is absent from the world,
    /// [`WorldError::DescriptorStorageMissing`] when a component named by an
    /// archetype has no storage column - the desync a partially applied hot
    /// reload can leave behind - [`WorldError::DescriptorSizeMismatch`] when
    /// two columns claiming one component id disagree on their row width, and
    /// whatever `attach` reports. Every one of them is raised before the first
    /// row moves, so a refused migration leaves the entity where it was.
    pub(crate) fn move_entity_to_archetype<F>(
        &mut self,
        entity: Entity,
        new_component_ids: Vec<ComponentId>,
        attach: F,
    ) -> Result<(), WorldError>
    where
        F: FnOnce(&mut ComponentColumns) -> Result<(), WorldError>,
    {
        let _zone = crate::profile_scope!(
            "move entity to archetype",
            [(
                "Entity being migrated: {:?}, New component type count: {}",
                entity,
                new_component_ids.len()
            )]
        );
        // Step 1: Resolve where the entity currently lives.
        let old_location = match self.entity_locations.get(&entity) {
            Some(loc) => *loc,
            None => return Err(WorldError::EntityNotFound),
        };

        let old_archetype_id = old_location.archetype_id;
        let old_index = old_location.index_in_archetype;

        // Step 2: Get or create the destination archetype and bail out if
        // the entity already lives there.
        let new_archetype_id = self.get_or_create_archetype(new_component_ids);

        // If same archetype, nothing to do (shouldn't happen for add_component)
        if old_archetype_id == new_archetype_id {
            warn!(
                target: pill_core::telemetry::telemetry_target::ECS,
                entity = ?entity,
                archetype_id = ?new_archetype_id,
                "entity already lives in the destination archetype; nothing to migrate"
            );
            return Ok(());
        }

        // Step 3: Take the source archetype out of the map for the duration of
        // the move. Moving a row needs `&mut` on both archetypes at once, which
        // the borrow checker cannot express through one `HashMap`; owning one
        // of them outright says what the previous pair of raw pointers said,
        // without the aliasing obligation. It goes back in Step 7 unless the
        // move emptied it.
        let Some(mut old_archetype) = self.archetypes.remove(&old_archetype_id) else {
            return Err(WorldError::ArchetypeMissing {
                entity,
                archetype_id: old_archetype_id,
            });
        };

        // Step 4: Verify the destination and reserve its rows before anything
        // moves. A component the destination archetype has no column for is the
        // manifest/storage desync `DescriptorStorageMissing` reports, and two
        // columns claiming one id at different widths is the reload equivalent;
        // finding both here is what keeps a failed migration atomic, because the
        // destination would otherwise already hold part of the entity's row
        // while the source still holds the entity. The reservation is here for
        // the same reason: afterwards no carried row can fail to land.
        let prepared = Self::prepare_destination_columns(
            &mut self.archetypes,
            &old_archetype,
            entity,
            old_index,
            new_archetype_id,
        );
        if let Err(error) = prepared {
            // Nothing has moved, so putting the source archetype back restores
            // the world exactly as the caller handed it over.
            self.archetypes.insert(old_archetype_id, old_archetype);
            return Err(error);
        }

        // Step 5: Write the entity's row into the destination. The attached
        // component goes first, while a refusal is still free; the carried rows
        // follow, and Step 4 has ruled out every way those can fail.
        let Some(new_archetype) = self.archetypes.get_mut(&new_archetype_id) else {
            self.archetypes.insert(old_archetype_id, old_archetype);
            return Err(WorldError::ArchetypeMissing {
                entity,
                archetype_id: new_archetype_id,
            });
        };

        let new_index = new_archetype.entities.len();
        new_archetype.entities.push(entity);

        if let Err(error) = attach(&mut new_archetype.component_storages) {
            // The entity is not yet anywhere but this list, and no row has
            // moved, so dropping its place undoes the whole attempt.
            new_archetype.entities.truncate(new_index);
            self.archetypes.insert(old_archetype_id, old_archetype);
            return Err(error);
        }

        let mut carried_result = Ok(());
        for component_id in &new_archetype.component_types {
            let Some(source) = old_archetype.component_storages.get_mut(*component_id) else {
                // Not carried: the destination gained this component, and
                // `attach` above wrote it.
                continue;
            };
            let Some(destination) = new_archetype.component_storages.get_mut(*component_id) else {
                // Step 4 proved every destination column exists.
                carried_result = Err(WorldError::DescriptorStorageMissing {
                    component_id: *component_id,
                    archetype_id: new_archetype_id,
                });
                break;
            };
            if let Err(error) = destination.take_row_from(source, old_index) {
                carried_result = Err(error);
                break;
            }
        }
        if let Err(error) = carried_result {
            // Unreachable after Step 4: the widths agree, the row exists and
            // every destination column has room. Should an invariant break get
            // here anyway, the source archetype goes back into the world so the
            // entities that are not being migrated keep their rows, and the
            // failure is reported rather than swallowed.
            self.archetypes.insert(old_archetype_id, old_archetype);
            return Err(error);
        }

        // Stamp the arrival tick on the component this migration attached.
        // A carried row brought its ticks with it - they are part of the row
        // the destination column took - so a component the source had needs
        // nothing here, and the two containers that used to be kept in
        // lockstep are now one.
        let current_tick = Tick::new(self.change_tick);
        for &component_id in &new_archetype.component_types {
            if old_archetype.component_storages.contains(component_id) {
                continue;
            }
            if let Some(column) = new_archetype.component_storages.get_mut(component_id) {
                column.set_row_ticks(new_index, ComponentTicks::new(current_tick));
            }
        }

        // Every destination column grew by exactly one row. A miss means
        // `attach` left a column it was responsible for short a row, which is
        // the desync Step 4 rules out for every other cause.
        debug_assert_eq!(
            new_archetype.entities.len(),
            new_index + 1,
            "the migrated entity is the destination's last row"
        );
        for &component_id in &new_archetype.component_types {
            if let Some(column) = new_archetype.component_storages.get(component_id) {
                debug_assert_eq!(
                    column.len(),
                    new_index + 1,
                    "a destination column did not grow by exactly one row"
                );
            }
        }

        self.entity_locations.insert(
            entity,
            EntityLocation {
                archetype_id: new_archetype_id,
                index_in_archetype: new_index,
            },
        );

        // Step 6: Remove the entity from the old archetype with swap_remove for
        // O(1) removal. A carried column was already swap-removed by the move
        // that took its row, so what is left is the entity list and the columns
        // the destination did not take - the components this migration drops,
        // which are the only rows that still need releasing.
        if old_index < old_archetype.entities.len() {
            // The columns first, while the destination is still reachable:
            // whether it has a column for a component is the same condition the
            // move above branched on, so it is what tells a carried column
            // apart from a dropped one.
            let destination_columns = self
                .archetypes
                .get(&new_archetype_id)
                .map(|archetype| &archetype.component_storages);
            for &component_id in &old_archetype.component_types {
                let carried =
                    destination_columns.is_some_and(|columns| columns.contains(component_id));
                if !carried {
                    if let Some(column) = old_archetype.component_storages.get_mut(component_id) {
                        column.swap_remove_discard(old_index);
                    }
                }
            }

            old_archetype.entities.swap_remove(old_index);

            // Update the location of the entity that was swapped (if any)
            if old_index < old_archetype.entities.len() {
                let swapped_entity = old_archetype.entities[old_index];
                if let Some(swapped_location) = self.entity_locations.get_mut(&swapped_entity) {
                    swapped_location.index_in_archetype = old_index;
                }
            }
        }

        // Step 7: Put the source archetype back, unless the move emptied it -
        // an empty archetype is dropped rather than kept, so its columns do not
        // hold allocations for a component set nothing uses.
        if old_archetype.entities.is_empty() {
            self.archetype_generation = self.archetype_generation.wrapping_add(1);
        } else {
            self.archetypes.insert(old_archetype_id, old_archetype);
        }

        Ok(())
    }

    /// Check that the destination archetype can host the entity's row, and
    /// reserve the space it needs, before the migration moves anything.
    ///
    /// Split out of [`Self::move_entity_to_archetype`] so the destination can
    /// be borrowed mutably while the source archetype - already out of the map
    /// by then - is borrowed immutably beside it.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::ArchetypeMissing`] when the destination archetype
    /// is absent, [`WorldError::DescriptorStorageMissing`] when it names a
    /// component it has no column for, [`WorldError::DescriptorSizeMismatch`]
    /// when a carried column's row width disagrees with the destination's,
    /// [`WorldError::DescriptorRowInvalid`] when the entity's recorded row is
    /// not in a carried column, and [`WorldError::DescriptorLayoutInvalid`]
    /// when a column cannot grow. Nothing is moved in any case; a reservation
    /// that already succeeded only grows a buffer, which no reader observes.
    fn prepare_destination_columns(
        archetypes: &mut HashMap<ArchetypeId, Archetype>,
        old_archetype: &Archetype,
        entity: Entity,
        old_index: usize,
        new_archetype_id: ArchetypeId,
    ) -> Result<(), WorldError> {
        let Some(new_archetype) = archetypes.get_mut(&new_archetype_id) else {
            return Err(WorldError::ArchetypeMissing {
                entity,
                archetype_id: new_archetype_id,
            });
        };
        // `component_types` and `component_storages` are separate fields, so
        // the list can be read while the columns beside it are written.
        let component_types: &[ComponentId] = &new_archetype.component_types;
        for &component_id in component_types {
            let source = old_archetype.component_storages.get(component_id);
            // A component both archetypes name but the source has no column for
            // cannot be carried, and nothing else would write it, so the
            // destination would end the migration one row short. That is the
            // desync a partially applied reload leaves behind, and it is the
            // one shape of it this migration cannot route around - unlike a
            // component being dropped, whose missing column simply has nothing
            // left to release.
            if source.is_none() && old_archetype.component_types.contains(&component_id) {
                return Err(WorldError::DescriptorStorageMissing {
                    component_id,
                    archetype_id: old_archetype.id,
                });
            }
            let Some(destination) = new_archetype.component_storages.get_mut(component_id) else {
                return Err(WorldError::DescriptorStorageMissing {
                    component_id,
                    archetype_id: new_archetype_id,
                });
            };
            if let Some(source) = source {
                if source.element_size() != destination.element_size() {
                    return Err(WorldError::DescriptorSizeMismatch);
                }
                if old_index >= source.len() {
                    return Err(WorldError::DescriptorRowInvalid);
                }
            }
            destination.reserve_row()?;
        }
        Ok(())
    }

    /// Remove an entity from the world completely
    ///
    /// This removes the entity from its archetype and updates all tracking structures.
    /// Returns true if the entity was found and removed, false otherwise.
    #[must_use]
    pub fn destroy_entity(&mut self, entity: Entity) -> bool {
        let _zone = crate::profile_scope!(
            "destroy entity",
            [("Target entity being destroyed: {:?}", entity)]
        );
        // Step 1: Remove the entity's location record; a missing record
        // means the entity is already gone.
        let location = match self.entity_locations.remove(&entity) {
            Some(loc) => loc,
            None => return false, // Entity doesn't exist
        };

        let archetype = match self.archetypes.get_mut(&location.archetype_id) {
            Some(arch) => arch,
            None => return false,
        };

        let old_index = location.index_in_archetype;

        // Step 2: swap_remove the entity and its component rows for O(1)
        // removal, updating the location of the entity swapped into its place.
        if old_index < archetype.entities.len() {
            archetype.entities.swap_remove(old_index);

            // Update the location of the entity that was swapped (if any)
            if old_index < archetype.entities.len() {
                let swapped_entity = archetype.entities[old_index];
                if let Some(swapped_location) = self.entity_locations.get_mut(&swapped_entity) {
                    swapped_location.index_in_archetype = old_index;
                }
            }

            // Also swap_remove from all component storages. A column releases
            // its own change ticks with the row, so this is one pass rather
            // than two. `component_types` and `component_storages` are separate
            // fields - Rust's split-borrow allows the shared slice alongside
            // mutable access to the columns - so no clone is needed.
            let component_type_ids: &[ComponentId] = &archetype.component_types;
            for component_id in component_type_ids {
                if let Some(column) = archetype.component_storages.get_mut(*component_id) {
                    column.swap_remove_discard(old_index);
                } else {
                    // A component with no column has no data to remove, so
                    // skipping is safe. The missing column is the
                    // manifest/storage desync reported by
                    // `WorldError::DescriptorStorageMissing`; report rather
                    // than panic, because this runs inside `process_frame`
                    // for managed projects.
                    warn!(
                        target: pill_core::telemetry::telemetry_target::ECS,
                        component_id = ?component_id,
                        archetype_id = ?archetype.id,
                        "destroy_entity: component has no storage column; skipping"
                    );
                }
            }
        }

        // Step 3: If the archetype is now empty, remove it entirely to
        // prevent memory leaks.
        let archetype_id = location.archetype_id;
        if archetype.entities.is_empty() {
            self.archetypes.remove(&archetype_id);
            self.archetype_generation = self.archetype_generation.wrapping_add(1);
        }

        // Step 4: Recycle the entity ID with an incremented generation so
        // the ID can be reused while stale handles are invalidated.
        //
        // A slot whose generation has reached `u32::MAX` is retired instead of
        // wrapping back to zero: wrapping would resurrect every stale handle
        // from 2^32 recycles ago - the classic ABA failure. Retirement is
        // effectively unreachable in practice (2^32 recycles of one slot), but
        // it is cheap to make the wrap impossible rather than silent.
        if entity.generation != u32::MAX {
            self.free_entity_ids
                .push((entity.id, entity.generation + 1));
        }

        true
    }

    /// Remove all empty archetypes from the world
    ///
    /// This cleans up archetypes that no longer contain any entities.
    /// Usually not necessary as empty archetypes can be reused, but useful for memory cleanup.
    pub fn cleanup_empty_archetypes(&mut self) {
        let empty_archetype_ids: Vec<ArchetypeId> = self
            .archetypes
            .iter()
            .filter(|(_, archetype)| archetype.entities.is_empty())
            .map(|(id, _)| *id)
            .collect();

        let _zone = crate::profile_scope!(
            "cleanup empty archetypes",
            [("removed: {}", empty_archetype_ids.len())]
        );
        for archetype_id in &empty_archetype_ids {
            self.archetypes.remove(archetype_id);
        }
        if !empty_archetype_ids.is_empty() {
            self.archetype_generation = self.archetype_generation.wrapping_add(1);
        }
    }

    /// Total number of entities alive in the world.
    #[inline]
    pub fn entity_count(&self) -> usize {
        self.entity_locations.len()
    }

    /// Iterate every archetype in the world, for read-only column access.
    ///
    /// The narrow seam an out-of-crate renderer needs. Collecting drawable
    /// entities means walking each archetype's columns, and `archetypes` is
    /// crate-private so the ECS internals cannot be mutated from outside. This
    /// hands out `&Archetype` only: the map itself, and every mutation path on
    /// it, stays sealed in this crate.
    ///
    /// Pairs with [`World::component_registry`], which resolves the columns
    /// found here by stable type name and size.
    #[inline]
    pub fn archetypes_iter(&self) -> impl Iterator<Item = &Archetype> {
        self.archetypes.values()
    }
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// ComponentInserter
// =============================================================================

/// Type-erased interface for pushing a single component value into storage.
///
/// `EntityBuilder` boxes one `ComponentInserter` per `.with(...)` call so
/// that the concrete component type is captured without the builder itself
/// being generic over every possible component.
trait ComponentInserter {
    /// Push the captured component value into the given storage.
    fn insert(self: Box<Self>, storage: &mut ComponentColumns);
    /// Return the [`ComponentId`] of the captured component type.
    fn component_id(&self) -> ComponentId;
}

/// Implementation of [`ComponentInserter`] that captures a concrete component type.
struct TypedComponentInserter<T: Component> {
    /// The component value to insert when the entity is built.
    component: T,
}

impl<T: Component> ComponentInserter for TypedComponentInserter<T> {
    fn insert(self: Box<Self>, storage: &mut ComponentColumns) {
        storage.column_of_mut::<T>().push::<T>(self.component);
    }

    fn component_id(&self) -> ComponentId {
        ComponentId::of::<T>()
    }
}

// =============================================================================
// EntityBuilder
// =============================================================================

/// Builder for constructing entities with components using a fluent API.
///
/// Returned by [`World::create_entity`]. Components are added with
/// [`with`](Self::with) and the entity is inserted into the world when
/// [`build`](Self::build) is called.
///
/// # Example
///
/// ```no_run
/// # use pill_engine::*;
/// # use trait_type_map::impl_trait_accessible;
/// # #[derive(Debug, Clone)] struct Transform { x: f32, y: f32, z: f32 }
/// # impl Component for Transform {}
/// # #[derive(Debug, Clone)] struct Velocity { x: f32, y: f32 }
/// # impl Component for Velocity {}
/// # impl_trait_accessible!(dyn Component; Transform, Velocity);
/// # let mut world = World::new();
/// world.create_entity()
///     .with(Transform { x: 0.0, y: 0.0, z: 0.0 })
///     .with(Velocity { x: 10.0, y: 0.0 })
///     .build().unwrap();
/// ```
pub struct EntityBuilder<'w> {
    /// World the built entity is inserted into on [`build`](Self::build).
    world: &'w mut World,
    /// Entity handle reserved by the free list for this build.
    entity: Entity,
    /// Type-erased components accumulated via [`with`](Self::with).
    components: Vec<Box<dyn ComponentInserter>>,
}

impl<'w> EntityBuilder<'w> {
    /// Add a component to the entity being built
    pub fn with<T>(mut self, component: T) -> Self
    where
        T: Component,
    {
        self.components
            .push(Box::new(TypedComponentInserter { component }));
        self
    }

    /// Finish building and insert the entity into the world.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError::ComponentNotRegistered`] if any of the component
    /// types added via [`.with()`](Self::with) were not registered with the
    /// world beforehand.
    pub fn build(self) -> Result<Entity, BuildError> {
        let component_ids: Vec<ComponentId> =
            self.components.iter().map(|c| c.component_id()).collect();
        let _zone = crate::profile_scope!(
            "entity build",
            [("Component types on entity: {}", component_ids.len())]
        );
        let entity = self.entity;

        // Validate that every component type is registered before we try to
        // create the archetype (which would panic on an unregistered type).
        for &id in &component_ids {
            if !self.world.storage_factories.contains_key(&id) {
                return Err(BuildError::ComponentNotRegistered { id });
            }
        }

        let components = self.components;
        self.world
            .insert_entity_with_components(entity, component_ids, |storage| {
                for inserter in components {
                    inserter.insert(storage);
                }
            });
        Ok(entity)
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod layout_tests {
    use super::*;

    /// Verifies that `EntityLocation` is 32 bytes with 16-byte alignment.
    #[test]
    fn entity_location_size() {
        assert_eq!(std::mem::size_of::<EntityLocation>(), 32);
        assert_eq!(std::mem::align_of::<EntityLocation>(), 16);
    }
}

#[cfg(test)]
mod tests;
