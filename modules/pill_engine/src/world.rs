//! Central ECS state container - entities, archetypes, components, and resources.
//!
//! # Responsibilities
//!
//! - Manages entity creation, destruction, and ID recycling via a free list.
//! - Owns all archetypes and provides the primary interface for component operations.
//! - Stores singleton resources with change-detection tick tracking.
//! - Provides random access to individual components via `get_component` / `get_component_mut`.
//! - Manages script component registration and per-frame update dispatch.
//!
//! # Design
//!
//! The [`World`] is the central hub of the ECS. It allocates entity IDs,
//! manages archetype storage, tracks entity-to-archetype mappings, and stores
//! resources (singleton data). Component types must be registered before use
//! so the world can assign bit indices for archetype mask matching. Entity
//! destruction recycles IDs through a free list with generation counters
//! to prevent dangling-handle bugs.

// Standard library
use std::collections::HashMap;

// External crates
use pill_core::{error, warn};

// Current crate
use crate::archetype::{
    validate_component_layout, Archetype, ArchetypeId, Blittability, ComponentColumns,
    ComponentLayout, FieldPlan, FieldSource, StorageFactory,
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
// Re-exports
// =============================================================================

pub use crate::error::{AddComponentError, BuildError, RemoveComponentError, WorldError};

// =============================================================================
// Registration headroom
// =============================================================================

/// Component-type headroom below which registration warns.
///
/// The 128-type ceiling is shared across a project and every optional module,
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
// Component Copier Type
// =============================================================================

/// Function that copies a component from one storage to another at given indices.
type ComponentCopier =
    fn(source: &ComponentColumns, destination: &mut ComponentColumns, index: usize);

// =============================================================================
// Script Updater Type
// =============================================================================

/// Function that updates a script component.
///
/// Takes: (storage, index, entity, world_ptr, commands_ptr).
/// Uses raw pointers to create a `ScriptContext` inside `update_scripts`.
///
/// SAFETY: The raw-pointer arguments (`world_ptr`, `commands_ptr`) are only
/// valid during the `update_scripts` call. Using a plain function pointer
/// (not a closure) guarantees that no state is captured and the callee
/// cannot stash the pointers for later use.
type ScriptUpdater = fn(&mut ComponentColumns, usize, Entity, *mut World, *mut CommandQueue);

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
    /// Component copiers for moving entities between archetypes
    pub(crate) component_copiers: HashMap<ComponentId, ComponentCopier>,
    /// Script component types (ComponentId, component mask bit)
    script_components: Vec<(ComponentId, u8)>,
    /// Script updaters for calling update() on script components
    script_updaters: HashMap<ComponentId, ScriptUpdater>,
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
            component_copiers: HashMap::new(),
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

    /// Reserve capacity for at least `additional` instances of component `T`
    /// in every archetype that currently contains `T`.
    ///
    /// This is a hint - the ECS may reserve more or less than requested
    /// depending on archetype distribution. Call after registering components
    /// and after creating archetypes you intend to populate.
    pub fn reserve_components<T>(&mut self, additional: usize)
    where
        T: Component,
    {
        let component_id = ComponentId::of::<T>();
        for archetype in self.archetypes.values_mut() {
            if archetype.component_types.contains(&component_id) {
                let storage = archetype.component_storages.column_of_mut::<T>();
                storage.reserve::<T>(additional);
                if let Some(ticks) = archetype.component_ticks.get_mut(&component_id) {
                    ticks.reserve(additional);
                }
            }
        }
    }

    /// Return one archetype-sized component chunk for language bindings.
    ///
    /// `chunk_index` is relative to archetypes containing `T`. The entity
    /// ID identifies the archetype shared by corresponding chunks of other
    /// component types.
    pub fn component_chunk_mut<T>(&mut self, chunk_index: usize) -> Option<(ArchetypeId, &mut [T])>
    where
        T: Component,
    {
        let component_id = ComponentId::of::<T>();
        let archetype = self
            .archetypes
            .values_mut()
            .filter(|archetype| archetype.component_types.contains(&component_id))
            .nth(chunk_index)?;
        let archetype_id = archetype.id;
        let storage = archetype.component_storages.column_of_mut::<T>();
        Some((archetype_id, storage.as_mut_slice::<T>()))
    }

    /// Return one component chunk together with its parallel change-tick column.
    ///
    /// Language bindings use this form when exposing writable component data.
    /// Both slices have identical lengths and row `i` in `ticks` describes row
    /// `i` in `components`.
    pub fn component_chunk_with_ticks_mut<T>(
        &mut self,
        chunk_index: usize,
    ) -> Option<(ArchetypeId, &mut [T], &mut [ComponentTicks])>
    where
        T: Component,
    {
        let component_id = ComponentId::of::<T>();
        let archetype = self
            .archetypes
            .values_mut()
            .filter(|archetype| archetype.component_types.contains(&component_id))
            .nth(chunk_index)?;
        let archetype_id = archetype.id;
        let components = archetype
            .component_storages
            .column_of_mut::<T>()
            .as_mut_slice::<T>();
        let Some(ticks_vec) = archetype.component_ticks.get_mut(&component_id) else {
            // `Archetype::new` creates a tick column for every entry in
            // `component_types`, so a component with storage but no ticks is
            // an internal-invariant break rather than a user error. Reporting
            // "no chunk" keeps the language-binding path from panicking
            // mid-call; the debug assertion names the offending pair.
            debug_assert!(
                false,
                "component {component_id:?} has storage but no tick column in \
                 archetype {archetype_id:?}"
            );
            return None;
        };
        let ticks = ticks_vec.as_mut_slice();
        debug_assert_eq!(components.len(), ticks.len());
        Some((archetype_id, components, ticks))
    }

    /// Return one component chunk from one already-known archetype, with its
    /// parallel change-tick column.
    ///
    /// The archetype-scoped twin of [`Self::component_chunk_with_ticks_mut`]:
    /// language bindings that hold an archetype identity (from a driver chunk)
    /// resolve the remaining query terms directly instead of scanning chunk
    /// indices until the archetypes match.
    pub fn component_chunk_with_ticks_mut_in_archetype<T>(
        &mut self,
        archetype_id: ArchetypeId,
    ) -> Option<(ArchetypeId, &mut [T], &mut [ComponentTicks])>
    where
        T: Component,
    {
        let component_id = ComponentId::of::<T>();
        let archetype = self.archetypes.get_mut(&archetype_id)?;
        if !archetype.component_types.contains(&component_id) {
            return None;
        }
        let components = archetype
            .component_storages
            .column_of_mut::<T>()
            .as_mut_slice::<T>();
        let Some(ticks_vec) = archetype.component_ticks.get_mut(&component_id) else {
            // Same invariant break the index-based twin documents: storage
            // without a tick column. Report "no chunk" instead of panicking
            // mid-boundary-call, and name the pair in debug builds.
            debug_assert!(
                false,
                "component {component_id:?} has storage but no tick column in \
                 archetype {archetype_id:?}"
            );
            return None;
        };
        let ticks = ticks_vec.as_mut_slice();
        debug_assert_eq!(components.len(), ticks.len());
        Some((archetype_id, components, ticks))
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
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

impl World {
    /// Register a component type with the World
    ///
    /// This must be called for each component type before it can be used.
    pub fn register_component<T>(&mut self)
    where
        T: Component + Clone,
    {
        self.register_component_inner::<T>(&[]);
    }

    /// Shared registration body for the layout-less and layout-carrying entry
    /// points.
    ///
    /// `fields` is the compile-time field layout when the caller has one. It
    /// is forwarded to the registry, which records it so a second registration
    /// of the same component can be checked against the first - a diagnostic
    /// for an ordinary component, and the soundness check for a shared one.
    ///
    /// # Shared components
    ///
    /// A component that declares [`Component::shared_name`] resolves to the
    /// same [`ComponentId`] in every binary that links it, so the second
    /// binary's registration finds the bit already taken and **binds to the
    /// existing column** rather than allocating a second one: the registry
    /// reports `AlreadyPresent`, no new bit is consumed, and entities spawned
    /// from either binary land in the same archetype. Nothing here special-
    /// cases that - it falls out of the id being equal - which is why there is
    /// no window in which two binaries share a column but not a mask bit.
    pub(crate) fn register_component_inner<T>(
        &mut self,
        fields: &'static [crate::component_registry::ComponentFieldDescriptor],
    ) where
        T: Component + Clone,
    {
        let _zone = crate::profile_scope!(
            "register component",
            [(
                "Component type being registered: {}",
                std::any::type_name::<T>()
            )]
        );
        let component_id = ComponentId::of::<T>();
        let type_name = crate::component::ComponentRegistry::registered_name::<T>();

        // Register component (bit index + name + layout)
        // `register_bit_with_layout` rather than `register`: the world does not
        // act on whether the type was already present, and re-registration is
        // normal here because a hot reload re-runs every `init`.
        let bit = match self
            .component_registry
            .register_bit_with_layout::<T>(fields)
        {
            Ok(bit) => bit,
            Err(error) => {
                // Neither the 128-type ceiling nor a shared-layout
                // disagreement is a programming error - both are outcomes of
                // what the user's binaries declare - so they are reported as
                // first-class diagnostics and recorded for the init entry
                // point instead of panicking. The caller (project/module init)
                // fails the reload transactionally when the error is drained
                // by `component_registry::register_all_components`.
                error!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    type_name = %type_name,
                    error = %error,
                    remaining = self.component_registry.available_slots(),
                    "component registration failed"
                );
                self.record_registration_error(error);
                return;
            }
        };
        // Warn while headroom is still available but getting thin, so
        // exhaustion is visible before it is fatal.
        let remaining = self.component_registry.available_slots();
        if remaining <= REGISTRATION_HEADROOM_WARNING_THRESHOLD {
            warn!(
                target: pill_core::telemetry::telemetry_target::ECS,
                remaining,
                type_name = %type_name,
                "component-type headroom is low; the 128-type ceiling is approaching"
            );
        }
        let _ = bit;

        // Record the registration chronologically so the host can enumerate
        // which types one module's init registered at all (plain or
        // persistable), which is how a type dropped from a reloaded module is
        // told apart from one merely downgraded to a plain component. The id is
        // recorded beside the name because a rebuilt image gets a fresh
        // `TypeId` per name: comparing ids is what tells a failed generation's
        // leftovers apart from a rollback generation's re-registration.
        self.component_registration_log.push((
            type_name,
            component_id,
            self.component_registration_sequence,
        ));
        self.component_registration_sequence = self.component_registration_sequence.wrapping_add(1);

        // Register the storage factory as plain DATA (type id, layout, and a
        // per-type function table) instead of a closure that would be
        // monomorphized into this generation's DLL. The engine builds the
        // actual column in `Archetype::new` as a concrete `Box<ErasedVecStorage>`
        // with no trait-object vtable, and re-homes its function table on
        // every reload, so columns survive DLL unloads.
        //
        // A shared component's column is built with `of_shared`, so its
        // element-type check compares layout instead of `TypeId`. That is the
        // point where the compiler stops vouching for the type and the
        // registry's layout check above takes over: the second binary's `T` is
        // a different `TypeId` for the same type, and only the recorded size,
        // alignment and schema hash establish that it really is.
        //
        // Re-registering replaces the factory, so the function table points at
        // the most recently loaded generation - the same last-writer-wins rule
        // the reload path already relies on, with `rehome_native_columns`
        // re-pointing live columns at it afterwards.
        let storage_info = if T::shared_name().is_some() {
            crate::archetype::NativeColumnInfo::of::<T>(true)
        } else {
            crate::archetype::NativeColumnInfo::of::<T>(false)
        };
        // Keep the table that is about to be replaced: the migration that
        // consumes old-layout columns must drop them through the glue that
        // wrote them, and by then this registration's table is the one stamped
        // on every column - see `migrate_component_column_in_place`.
        if let Some(StorageFactory::Native(previous)) = self.storage_factories.get(&component_id) {
            self.retired_native_storage_ops
                .insert(component_id, previous.ops);
        }
        self.storage_factories
            .insert(component_id, StorageFactory::Native(storage_info));

        // Register copier function for this component type.
        // Uses a named generic function (not a closure) so the fn pointer
        // requires no heap allocation or vtable dispatch.
        self.component_copiers
            .insert(component_id, copy_component::<T>);
    }

    /// Drain the first registration failure recorded by
    /// [`Self::register_component`], if any.
    ///
    /// The artifact-wide registration loop calls this after running every
    /// descriptor so the generated `init` can fail the reload with a non-zero
    /// status when the component-type ceiling was hit — a diagnosable startup
    /// failure rather than a silent half-registration.
    pub fn take_registration_error(&mut self) -> Option<WorldError> {
        self.registration_error.take()
    }

    /// Record a registration failure raised outside `register_component`, so
    /// the artifact-wide registration loop fails the reload the same way it
    /// does for the component-type ceiling.
    ///
    /// The first failure wins: later registrations in the same pass are often
    /// knock-on effects of the first, and the original is the diagnosable one.
    pub(crate) fn record_registration_error(&mut self, error: WorldError) {
        if self.registration_error.is_none() {
            self.registration_error = Some(error);
        }
    }

    /// Register a component together with its compile-time field layout, so
    /// the C# mirror codegen can emit a typed struct. Components registered
    /// without field metadata (hand-registered, descriptor, or unit types) keep
    /// the opaque ABI-blob mirror.
    pub fn register_component_with_layout<T>(
        &mut self,
        fields: &'static [crate::component_registry::ComponentFieldDescriptor],
    ) where
        T: Component + Clone,
    {
        self.register_component_inner::<T>(fields);
        self.component_field_layouts.insert(
            ComponentId::of::<T>(),
            ComponentFieldLayout::from_static(fields),
        );
    }

    /// Return the field layout a component was registered with.
    ///
    /// `None` for components registered without field metadata, which is how
    /// the C# codegen decides between a typed mirror and the ABI blob, and how
    /// the editor decides a component is not field-editable.
    pub fn component_field_layout(
        &self,
        component_id: ComponentId,
    ) -> Option<&[crate::component_registry::ComponentFieldDescriptor]> {
        self.component_field_layouts
            .get(&component_id)
            .map(ComponentFieldLayout::fields)
    }

    /// Record a runtime-described layout for a descriptor component.
    ///
    /// Overwrites any previous layout for the same id, so a manifest reload
    /// replaces rather than accumulates. Used by the C# backend so managed
    /// components become field-inspectable in the editor.
    ///
    /// # Errors
    ///
    /// Returns [`ComponentFieldError::UnsupportedField`] for a descriptor that
    /// reaches past the component's registered size or names a container tag.
    /// A descriptor row is raw bytes with no Rust value in it, so the accessors
    /// have to be able to trust both: the bound keeps them inside the row, and
    /// the tag check keeps them from following a `(pointer, length)` pair the
    /// row never held.
    pub fn register_component_descriptor_with_layout(
        &mut self,
        component_id: ComponentId,
        fields: Vec<crate::component_registry::ComponentFieldDescriptor>,
    ) -> Result<(), crate::component_field::ComponentFieldError> {
        let component_size = self.component_registry.get_size(&component_id);
        for field in &fields {
            let Some(end) = field.offset.checked_add(field.size) else {
                return Err(
                    crate::component_field::ComponentFieldError::UnsupportedField {
                        field: field.name.to_string(),
                        reason: "the field's range overflows",
                    },
                );
            };
            if component_size.is_some_and(|size| end > size) {
                return Err(
                    crate::component_field::ComponentFieldError::UnsupportedField {
                        field: field.name.to_string(),
                        reason: "the field extends past the component's registered size",
                    },
                );
            }
            if !component_id.is_native_storage()
                && crate::component_field::is_container_tag(field.type_tag)
            {
                return Err(crate::component_field::ComponentFieldError::UnsupportedField {
                    field: field.name.to_string(),
                    reason: "a container tag is read as a native pointer and length, which a descriptor row does not hold",
                });
            }
        }
        self.component_field_layouts
            .insert(component_id, ComponentFieldLayout::from_owned(fields));
        Ok(())
    }

    /// Re-home every native column's per-type function table.
    ///
    /// Called by the host after each generation's `init` and before migration.
    /// Columns store function pointers into the DLL that created them; when
    /// that DLL is evicted from the reload graveyard the pointers dangle. This
    /// pass refreshes each column from the latest factory registered for its
    /// component id, so:
    ///
    /// - unchanged types point at the freshly loaded generation (still mapped),
    /// - schema-changed types point at the previous generation (still mapped
    ///   while the migration consumes and drops their columns),
    /// - type-erased (foreign-language) columns are untouched.
    pub fn rehome_native_columns(&mut self) {
        // Step 1: Snapshot the current per-type function tables first so the
        // immutable borrow of `storage_factories` cannot conflict with the
        // mutable borrow of `archetypes` below.
        let factory_ops: HashMap<ComponentId, crate::archetype::ColumnOps> = self
            .storage_factories
            .iter()
            .filter_map(|(component_id, factory)| match factory {
                StorageFactory::Native(info) => Some((*component_id, info.ops)),
                StorageFactory::Descriptor(_) => None,
            })
            .collect();

        // Step 2: Refresh every column whose component id has a native factory.
        for archetype in self.archetypes.values_mut() {
            for &component_id in &archetype.component_types {
                let Some(&ops) = factory_ops.get(&component_id) else {
                    continue;
                };
                if let Some(column) = archetype.component_storages.get_mut(component_id) {
                    column.refresh_ops(ops);
                }
            }
        }
    }

    /// Native columns the world still stores although no factory describes
    /// their component id.
    ///
    /// A column outlives its registration when a type is re-registered under a
    /// fresh id - the rebuilt image's "same name, fresh `TypeId`" case - and
    /// the old id is never swept: the archetype keeps the id in its mask and
    /// the column in its storage, but no factory is left to describe the type.
    /// The host's reload path uses this as the gate on unmapping a retired
    /// image, because a column's drop table lives in the image that produced
    /// it.
    pub fn columns_without_factory(&self) -> usize {
        self.archetypes
            .values()
            .flat_map(|archetype| archetype.component_types.iter())
            .filter(|component_id| {
                component_id.is_native_storage()
                    && !self.storage_factories.contains_key(component_id)
            })
            .count()
    }

    /// Drop every native column whose component id has no factory left.
    ///
    /// The id-keyed counterpart of `drop_forgotten_components`: the name-keyed
    /// sweep cannot reach these, because the name resolves to whatever
    /// registration claimed it last while the stale column keeps the id the
    /// previous generation minted. Each affected component is removed from
    /// every entity that carries it, which empties the archetypes holding it
    /// and drops their columns through the table that is still mapped while
    /// this runs. Returns the number of orphaned component ids swept.
    pub fn drop_columns_without_factory(&mut self) -> usize {
        // Step 1: Collect the orphaned ids across every archetype. Deduplicated
        // by hand: the list is a handful of entries at most, and a `HashSet`
        // would hide the ordering.
        let mut orphaned_ids: Vec<ComponentId> = Vec::new();
        for archetype in self.archetypes.values() {
            for &component_id in &archetype.component_types {
                if component_id.is_native_storage()
                    && !self.storage_factories.contains_key(&component_id)
                    && !orphaned_ids.contains(&component_id)
                {
                    orphaned_ids.push(component_id);
                }
            }
        }

        // Step 2: Remove each orphaned component from every entity carrying
        // it. The entities are collected up front so the mutable borrows
        // during removal never overlap the iteration, exactly as in
        // `drop_forgotten_components`.
        for component_id in &orphaned_ids {
            let entities: Vec<Entity> = self
                .entity_locations
                .iter()
                .filter(|(_, location)| {
                    self.archetypes
                        .get(&location.archetype_id)
                        .is_some_and(|archetype| archetype.component_types.contains(component_id))
                })
                .map(|(entity, _)| *entity)
                .collect();
            for entity in entities {
                let _ = self.remove_component_by_id(entity, *component_id);
            }
        }
        orphaned_ids.len()
    }

    /// Register an unmanaged component described by an external language.
    ///
    /// Re-registering an identical layout and name for the same `stable_id`
    /// is idempotent and returns the existing [`ComponentId`].
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DescriptorStableIdZero`] for a zero `stable_id`,
    /// [`WorldError::DescriptorSizeZero`] for a zero `size`,
    /// [`WorldError::DescriptorAlignmentInvalid`] for a zero or non-power-of-two
    /// `align`, [`WorldError::DescriptorLayoutInvalid`] for an oversized layout,
    /// [`WorldError::DescriptorAlreadyRegistered`] when the `stable_id` is
    /// already taken by a different layout or name, and
    /// [`WorldError::ComponentTypeLimitExceeded`] when the registry is full.
    ///
    /// `blittability` is the caller's evidence that every field of the
    /// component is a blittable value type. It is required, not optional:
    /// the storage built from this layout copies rows with `ptr::copy`, frees
    /// them without running element destructors, and is shared across threads,
    /// so there is no check this function could run itself that would make an
    /// owning layout safe. [`Blittability`] documents what each constructor
    /// proves.
    pub fn register_component_descriptor(
        &mut self,
        stable_id: u128,
        name: impl Into<String>,
        size: usize,
        align: usize,
        schema_hash: u64,
        blittability: Blittability,
    ) -> Result<ComponentId, WorldError> {
        if stable_id == 0 {
            return Err(WorldError::DescriptorStableIdZero);
        }
        // The same three checks a relayout runs, so a layout this refuses can
        // never be one the storage would accept later.
        validate_component_layout(size, align)?;
        let name = name.into();
        let component_id = ComponentId::descriptor(stable_id);
        if let Some(existing) = self.storage_factories.get(&component_id) {
            return match existing {
                StorageFactory::Descriptor(layout)
                    if layout.size == size
                        && layout.align == align
                        && layout.schema_hash == schema_hash
                        && self.component_registry.get_name(&component_id) == Some(&name) =>
                {
                    Ok(component_id)
                }
                _ => Err(WorldError::DescriptorAlreadyRegistered),
            };
        }
        // A name already claimed by a live column belongs to a different
        // component, not to an earlier generation of this one: a manifest
        // reload derives `stable_id` from the same full name, so it lands on
        // the idempotent path above rather than here. Registering a second
        // component under that name would make every later lookup by name
        // ambiguous, and managed code binds by name, so the collision is
        // reported now instead of producing a binding to the wrong column.
        if let Some((existing_id, live_rows)) = self.live_component_with_name(&name, component_id) {
            return Err(WorldError::ComponentNameCollision {
                type_name: name,
                existing_id,
                incoming_id: component_id,
                live_rows,
            });
        }

        // The registry reports the 128-type ceiling as a typed error (with the
        // offending name and current count) rather than panicking; propagate it.
        self.component_registry
            .register_descriptor(stable_id, name, size)?;
        self.storage_factories.insert(
            component_id,
            StorageFactory::Descriptor(ComponentLayout::new(
                size,
                align,
                schema_hash,
                blittability,
            )?),
        );
        Ok(component_id)
    }

    /// Replace a descriptor component's layout, migrating every stored row.
    ///
    /// The one way a descriptor component's storage shape may change after
    /// registration. The component keeps its id, its registry bit and its
    /// archetype membership - a fresh registration would allocate a new bit
    /// index, and that index is baked into archetype masks and scheduled access
    /// masks - so only the columns' element layout and the recorded size move.
    ///
    /// `plan` says where each byte of a new row comes from
    /// ([`FieldPlan::between`] builds one from two field lists). Anything
    /// it does not cover is left zero. Row order and change ticks are
    /// preserved, so entity locations stay valid and no system observes a
    /// spurious `Added`.
    ///
    /// The new layout and the plan are checked before the first column is
    /// touched, so a rejected call leaves the world exactly as it was.
    ///
    /// Returns the number of rows migrated.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DescriptorSizeZero`],
    /// [`WorldError::DescriptorAlignmentInvalid`] or
    /// [`WorldError::DescriptorLayoutInvalid`] when the new layout cannot describe
    /// storage, [`WorldError::DescriptorComponentNotRegistered`] when the id is not
    /// a registered descriptor component, [`WorldError::DescriptorStorageMissing`]
    /// when an archetype lists the component without a column,
    /// [`WorldError::ComponentColumnLayoutMismatch`] when a column's element size
    /// disagrees with the registered layout, and [`WorldError::DescriptorRowInvalid`]
    /// when a planned field falls outside a row of either layout.
    pub fn relayout_descriptor_component(
        &mut self,
        component_id: ComponentId,
        size: usize,
        align: usize,
        schema_hash: u64,
        plan: &FieldPlan,
    ) -> Result<usize, WorldError> {
        validate_component_layout(size, align)?;

        // The previous layout is what the plan's source offsets are measured
        // against, so it has to be the one the columns are actually using.
        let Some(StorageFactory::Descriptor(previous)) = self.storage_factories.get(&component_id)
        else {
            return Err(WorldError::DescriptorComponentNotRegistered { id: component_id });
        };
        let previous_size = previous.size;
        plan.validate(previous_size, size)?;

        // Every archetype holding the component, including the ones with no
        // rows: their columns must carry the new element layout so the next
        // entity added to them is stored at the new shape.
        // A relayout keeps the witness and the release hook: the shape
        // changes, the promise about what a row owns does not.
        let layout = ComponentLayout {
            size,
            align,
            schema_hash,
            blittability: previous.blittability,
        };

        // Step 1: Verify every column before touching one. The plan and the
        // factory were checked above; what remains is the possibility that a
        // column disagrees with the factory - the desync a partially applied
        // relayout used to produce - so the whole set is checked first and the
        // report names the column that disagrees.
        let mut targets = Vec::new();
        for (archetype_id, archetype) in &self.archetypes {
            if !archetype.component_types.contains(&component_id) {
                continue;
            }
            let Some(column) = archetype.component_storages.get(component_id) else {
                return Err(WorldError::DescriptorStorageMissing {
                    component_id,
                    archetype_id: *archetype_id,
                });
            };
            if column.element_size() != previous_size {
                return Err(WorldError::ComponentColumnLayoutMismatch {
                    component_id,
                    archetype_id: *archetype_id,
                    expected: previous_size,
                    actual: column.element_size(),
                });
            }
            targets.push(*archetype_id);
        }

        // Step 2: Migrate. Every column here was verified a moment ago and
        // nothing in between reshapes the map, so the lookups cannot miss.
        let mut migrated = 0;
        for archetype_id in targets {
            let archetype = self
                .archetypes
                .get_mut(&archetype_id)
                .expect("the verification pass listed only present archetypes");
            let column = archetype
                .component_storages
                .get_mut(component_id)
                .expect("the verification pass checked the column is present");
            migrated += column.relayout_validated(layout.clone(), plan, previous_size);
        }

        // Publish the new layout to everyone who reads it back: the storage
        // factory the chunk accessors consult, and the registry copy the
        // diagnostics and later registrations compare against. Both records
        // move together, or `get_layout` keeps reporting the placeholder
        // alignment registration started with.
        self.component_registry
            .update_descriptor_layout(&component_id, &layout);
        self.storage_factories
            .insert(component_id, StorageFactory::Descriptor(layout));
        Ok(migrated)
    }

    /// Move every row of one descriptor component into another and retire the
    /// source registration.
    ///
    /// The rename half of the managed manifest's storage story: a successor
    /// registration derives its id from its own name, so rows cannot stay put
    /// the way a relayout keeps them. Each row is reshaped through `plan` - the
    /// same [`FieldPlan`] a relayout uses, measured from the source's field
    /// layout to the successor's - and added to the successor before it is
    /// removed from the source, so an entity whose only component this is
    /// survives instead of being destroyed when the removal empties it.
    ///
    /// The source registration is retired once every row has moved: its name
    /// claim, registry entry and storage factory go, and its bit is released
    /// when no archetype with live rows still references it. A second remap out
    /// of the source is therefore impossible, which is what the caller wants:
    /// the source no longer exists.
    ///
    /// Returns the number of rows moved.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DescriptorRemapSelf`] when both ids are equal,
    /// [`WorldError::DescriptorComponentNotRegistered`] when either id is not a
    /// registered descriptor component,
    /// [`WorldError::DescriptorComponentAlreadyPresent`] when the successor
    /// already carries rows (a fresh registration has none; merging two row
    /// sets would make the plan's source side wrong for half its input),
    /// [`WorldError::DescriptorRowInvalid`] when the plan does not fit the two
    /// layouts, and [`WorldError::DescriptorStorageMissing`] when an archetype
    /// lists the source without a column.
    pub fn remap_descriptor_component(
        &mut self,
        old_component_id: ComponentId,
        new_component_id: ComponentId,
        plan: &FieldPlan,
    ) -> Result<usize, WorldError> {
        if old_component_id == new_component_id {
            return Err(WorldError::DescriptorRemapSelf);
        }
        let old_size = match self.storage_factories.get(&old_component_id) {
            Some(StorageFactory::Descriptor(layout)) => layout.size,
            _ => {
                return Err(WorldError::DescriptorComponentNotRegistered {
                    id: old_component_id,
                })
            }
        };
        let new_size = match self.storage_factories.get(&new_component_id) {
            Some(StorageFactory::Descriptor(layout)) => layout.size,
            _ => {
                return Err(WorldError::DescriptorComponentNotRegistered {
                    id: new_component_id,
                })
            }
        };
        plan.validate(old_size, new_size)?;
        if self.live_row_count(new_component_id) != 0 {
            return Err(WorldError::DescriptorComponentAlreadyPresent);
        }

        // Step 1: Read and reshape every row before the first mutation, so a
        // row the plan cannot describe fails before anything has moved. The
        // plan was validated against the registered sizes, so the slices in
        // here are in bounds.
        let mut moves: Vec<(Entity, Vec<u8>)> = Vec::new();
        for (archetype_id, archetype) in &self.archetypes {
            if !archetype.component_types.contains(&old_component_id) {
                continue;
            }
            let Some(column) = archetype.component_storages.get(old_component_id) else {
                return Err(WorldError::DescriptorStorageMissing {
                    component_id: old_component_id,
                    archetype_id: *archetype_id,
                });
            };
            for (row, entity) in archetype.entities.iter().enumerate() {
                let old_bytes = column.bytes(row).ok_or(WorldError::DescriptorRowInvalid)?;
                let mut new_bytes = vec![0_u8; new_size];
                for planned in plan.fields() {
                    let FieldSource::OldOffset(source_offset) = planned.source else {
                        continue;
                    };
                    new_bytes[planned.offset..planned.offset + planned.bytes]
                        .copy_from_slice(&old_bytes[source_offset..source_offset + planned.bytes]);
                }
                moves.push((*entity, new_bytes));
            }
        }

        // Step 2: Move every row. Add first, remove second: the two columns
        // coexist for a moment and the entity never loses its last component.
        let mut moved_rows = 0;
        for (entity, new_bytes) in moves {
            self.add_descriptor_component(entity, new_component_id, &new_bytes)?;
            self.remove_component_by_id(entity, old_component_id)
                .map_err(|_| WorldError::DescriptorComponentMissing)?;
            moved_rows += 1;
        }

        // Step 3: The source holds no rows now; retire it so its name can be
        // declared again and its bit returns to the pool. Every archetype that
        // carried it was emptied by Step 2, which is the precondition the
        // retirement's bit rule checks.
        self.retire_component_registration(old_component_id);
        Ok(moved_rows)
    }

    /// Return a raw descriptor component column for language bindings.
    pub fn descriptor_component_chunk_mut(
        &mut self,
        component_id: ComponentId,
        chunk_index: usize,
    ) -> Option<(ArchetypeId, *mut u8, usize, &mut [ComponentTicks])> {
        let archetype = self
            .archetypes
            .values_mut()
            .filter(|archetype| archetype.component_types.contains(&component_id))
            .nth(chunk_index)?;
        let archetype_id = archetype.id;
        let column = archetype.component_storages.get_mut(component_id)?;
        let len = column.len();
        let data = column.as_mut_ptr();
        let ticks = archetype
            .component_ticks
            .get_mut(&component_id)?
            .as_mut_slice();
        debug_assert_eq!(len, ticks.len());
        Some((archetype_id, data, len, ticks))
    }

    /// Return a raw native component column for language bindings.
    ///
    /// The native twin of [`Self::descriptor_component_chunk_mut`]: returns the
    /// contiguous row buffer of a native (Rust-registered) component as raw
    /// bytes, so the C# backend can expose components that an optional module
    /// registered without naming their concrete Rust type. Only native
    /// components are served; descriptor components must use
    /// [`Self::descriptor_component_chunk_mut`] instead.
    /// The returned pointer is only valid for the active managed-system
    /// invocation and must not be retained beyond it.
    pub fn native_component_chunk_mut(
        &mut self,
        component_id: ComponentId,
        chunk_index: usize,
    ) -> Option<(ArchetypeId, *mut u8, usize, usize, &mut [ComponentTicks])> {
        if !component_id.is_native_storage() {
            return None;
        }
        let archetype = self
            .archetypes
            .values_mut()
            .filter(|archetype| archetype.component_types.contains(&component_id))
            .nth(chunk_index)?;
        let archetype_id = archetype.id;
        let column = archetype.component_storages.get_mut(component_id)?;
        let len = column.len();
        let data = column.as_mut_ptr();
        let element_size = column.elem_size();
        let ticks = archetype
            .component_ticks
            .get_mut(&component_id)?
            .as_mut_slice();
        debug_assert_eq!(len, ticks.len());
        Some((archetype_id, data, len, element_size, ticks))
    }

    /// Return a raw descriptor component column from one already-known archetype.
    ///
    /// The archetype-scoped twin of [`Self::descriptor_component_chunk_mut`].
    pub fn descriptor_component_chunk_in_archetype(
        &mut self,
        component_id: ComponentId,
        archetype_id: ArchetypeId,
    ) -> Option<(ArchetypeId, *mut u8, usize, &mut [ComponentTicks])> {
        let archetype = self.archetypes.get_mut(&archetype_id)?;
        let column = archetype.component_storages.get_mut(component_id)?;
        let len = column.len();
        let data = column.as_mut_ptr();
        let ticks = archetype
            .component_ticks
            .get_mut(&component_id)?
            .as_mut_slice();
        debug_assert_eq!(len, ticks.len());
        Some((archetype_id, data, len, ticks))
    }

    /// Return a raw native component column from one already-known archetype.
    ///
    /// The archetype-scoped twin of [`Self::native_component_chunk_mut`].
    pub fn native_component_chunk_in_archetype(
        &mut self,
        component_id: ComponentId,
        archetype_id: ArchetypeId,
    ) -> Option<(ArchetypeId, *mut u8, usize, usize, &mut [ComponentTicks])> {
        if !component_id.is_native_storage() {
            return None;
        }
        let archetype = self.archetypes.get_mut(&archetype_id)?;
        if !archetype.component_types.contains(&component_id) {
            return None;
        }
        let column = archetype.component_storages.get_mut(component_id)?;
        let len = column.len();
        let data = column.as_mut_ptr();
        let element_size = column.elem_size();
        let ticks = archetype
            .component_ticks
            .get_mut(&component_id)?
            .as_mut_slice();
        debug_assert_eq!(len, ticks.len());
        Some((archetype_id, data, len, element_size, ticks))
    }

    /// Entity IDs that were destroyed and are waiting to be handed out again.
    ///
    /// The engine's own retirement pool: an ID here belongs to no live entity,
    /// but the slot is kept so a later spawn reuses it with a bumped
    /// generation rather than growing the ID space.
    pub fn recycled_entity_id_count(&self) -> usize {
        self.free_entity_ids.len()
    }

    /// Number of resources the world holds.
    pub fn resource_count(&self) -> usize {
        self.resources.len()
    }

    /// Total number of rows every archetype currently stores for one
    /// component id.
    ///
    /// Zero means the component is registered but nothing is using it, which
    /// is what a superseded hot-reload generation looks like once its entities
    /// have migrated away. A non-zero count means the column is still live, so
    /// its registration cannot be treated as stale and discarded.
    pub fn live_row_count(&self, component_id: ComponentId) -> usize {
        self.archetypes
            .values()
            .filter(|archetype| archetype.component_types.contains(&component_id))
            .map(|archetype| archetype.entities.len())
            .sum()
    }

    /// Whether any archetype still lists this component id.
    ///
    /// The host's failed-generation cleanup uses this to assert, in debug
    /// builds, that a stranded id is really gone - rows, columns and tables -
    /// before the image that defined it is retired from the graveyard.
    pub fn any_archetype_lists_component(&self, component_id: ComponentId) -> bool {
        self.archetypes
            .values()
            .any(|archetype| archetype.component_types.contains(&component_id))
    }

    /// The first registered component other than `excluding` that claims
    /// `type_name` and still holds rows, with that row count.
    ///
    /// Both collision guards ask the same question: is a same-name
    /// registration a superseded generation or a live peer? Live rows answer
    /// part of it - a peer's rows are the ones that would be lost - but they do
    /// not separate the two cases on their own, because a generation being
    /// retired still holds its rows while its replacement registers. The host
    /// therefore announces the retiring generation's names before init (see
    /// [`Self::supersede_persist_registrations`]); this lookup supplies the
    /// candidate the announcement is judged against.
    pub(crate) fn live_component_with_name(
        &self,
        type_name: &str,
        excluding: ComponentId,
    ) -> Option<(ComponentId, usize)> {
        self.component_registry
            .registered_components()
            .filter(|(id, _, name)| *name == type_name && *id != excluding)
            .map(|(id, _, _)| (id, self.live_row_count(id)))
            .find(|(_, live_rows)| *live_rows > 0)
    }

    /// Every registered component that claims `type_name`, in registration
    /// order.
    ///
    /// Unlike [`Self::live_component_with_name`] this neither requires live
    /// rows nor excludes an id. A rebuilt image usually gets the compiler's id
    /// back for an unchanged type, so a superseding registration's predecessor
    /// is often the entry *with* the incoming id, and older generations can
    /// still be listed under the same name - every candidate has to be
    /// considered, not one of them.
    pub(crate) fn component_ids_with_name(&self, type_name: &str) -> Vec<ComponentId> {
        self.component_registry
            .registered_components()
            .filter(|(_, _, name)| *name == type_name)
            .map(|(id, _, _)| id)
            .collect()
    }

    /// Resolve a component ID from its registered type name, without the
    /// persistable-only filter.
    ///
    /// Used by the C# backend to map an optional module's exposed component
    /// name (e.g. `pill_spline::Spline`) to its native [`ComponentId`] so a
    /// byte-level binding can be created without naming the concrete type.
    ///
    /// Unlike the persistable resolver, this has no `persist_inserters` filter
    /// to collapse the candidate set, so a name claimed by two registrations
    /// arrives here with both still visible. It used to break that tie with
    /// `max_by_key(bit)`, which is not a recency ordering - `allocate_bit`
    /// reissues bits reclaimed by `remove` before advancing `next_bit`, so the
    /// highest bit can belong to the older registration. Binding managed code
    /// to the wrong column that way is silent, so an ambiguous name is now
    /// reported as [`WorldError::ComponentNameAmbiguous`] instead.
    ///
    /// A component declared with a shared identity cannot reach this state:
    /// every binary that links it computes the same [`ComponentId`], so there
    /// is only ever one registration to find.
    pub fn resolve_component_id_by_name_any(
        &self,
        type_name: &str,
    ) -> Result<Option<ComponentId>, WorldError> {
        let mut candidates = self
            .component_registry
            .registered_components()
            .filter(|(_, _, name)| *name == type_name)
            .map(|(id, _, _)| id);

        let Some(first) = candidates.next() else {
            return Ok(None);
        };
        let extra = candidates.count();
        if extra > 0 {
            return Err(WorldError::ComponentNameAmbiguous {
                type_name: type_name.to_string(),
                count: extra + 1,
            });
        }
        Ok(Some(first))
    }

    /// Return the byte size and alignment of a registered component's layout.
    ///
    /// Works for both native (Rust) and descriptor (foreign-language) components.
    /// Used by the C# backend to validate that a managed mirror struct has the
    /// same ABI layout as the component an optional module registered.
    pub fn component_layout(&self, component_id: ComponentId) -> Option<(usize, usize)> {
        match self.storage_factories.get(&component_id) {
            Some(StorageFactory::Native(info)) => Some((info.size, info.align)),
            Some(StorageFactory::Descriptor(layout)) => Some((layout.size, layout.align)),
            None => None,
        }
    }

    /// Every live entity with its component type names, sorted by entity id.
    ///
    /// Read-only and tick-neutral: nothing is mutated and no change tick is
    /// touched, so the editor can call this every refresh without disturbing
    /// the simulation. Component names are resolved per archetype once and
    /// shared across the entities of that archetype.
    pub fn entity_rows(&self) -> Vec<EntityRow> {
        // Step 1: Resolve each archetype's component names once; the string
        // work is per archetype rather than per entity.
        let mut archetype_names: HashMap<ArchetypeId, Vec<String>> =
            HashMap::with_capacity(self.archetypes.len());
        for (archetype_id, archetype) in &self.archetypes {
            let names: Vec<String> = archetype
                .component_types
                .iter()
                .filter_map(|component_id| {
                    self.component_registry
                        .get_name(component_id)
                        .map(str::to_string)
                })
                .collect();
            archetype_names.insert(*archetype_id, names);
        }

        // Step 2: Assemble one row per live entity, cloning the shared names.
        let mut rows: Vec<EntityRow> = self
            .entity_locations
            .iter()
            .map(|(entity, location)| EntityRow {
                entity: *entity,
                components: archetype_names
                    .get(&location.archetype_id)
                    .cloned()
                    .unwrap_or_default(),
            })
            .collect();

        // Step 3: Deterministic ordering for the editor list.
        rows.sort_by_key(|row| row.entity.id());
        rows
    }

    /// Component type names attached to one entity, or `None` when it is dead.
    ///
    /// Names come from the entity's own archetype, never from a registry scan,
    /// so the result is authoritative for the generation that created the data.
    pub fn entity_component_names(&self, entity: Entity) -> Option<Vec<String>> {
        let location = self.entity_locations.get(&entity)?;
        let archetype = self.archetypes.get(&location.archetype_id)?;
        Some(
            archetype
                .component_types
                .iter()
                .filter_map(|component_id| {
                    self.component_registry
                        .get_name(component_id)
                        .map(str::to_string)
                })
                .collect(),
        )
    }

    /// Every registered component type: name plus current [`ComponentId`].
    ///
    /// Names are the stable cross-reload key; ids are per-generation and must
    /// not be cached across a reload. Used by the editor's add-component
    /// picker, not by the per-frame snapshot.
    pub fn registered_components(&self) -> Vec<(String, ComponentId)> {
        let mut components: Vec<(String, ComponentId)> = self
            .component_registry
            .registered_components()
            .map(|(component_id, _, name)| (name.to_string(), component_id))
            .collect();
        components.sort();
        components
    }

    /// Whether a component type is registered as persistable (schema-migrated
    /// across reloads). Used by the editor to mark such components.
    pub fn component_is_persistable(&self, component_id: ComponentId) -> bool {
        self.persist_inserters.contains_key(&component_id)
    }

    /// The component id an entity's archetype actually stores for a registered
    /// type name.
    ///
    /// Unlike [`Self::resolve_component_id_by_name_any`], which searches every
    /// generation the registry still remembers, this looks only at the columns
    /// the entity really has. That makes it correct across reloads even when a
    /// bit index was recycled, and it guarantees the returned id has a live
    /// column, a live tick vector, and a field layout belonging to the
    /// generation that created the data.
    pub fn resolve_entity_component_id(
        &self,
        entity: Entity,
        type_name: &str,
    ) -> Option<ComponentId> {
        let location = self.entity_locations.get(&entity)?;
        let archetype = self.archetypes.get(&location.archetype_id)?;
        archetype
            .component_types
            .iter()
            .copied()
            .find(|component_id| self.component_registry.get_name(component_id) == Some(type_name))
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
            if !archetype.component_storages.contains(*id)
                || !archetype.component_ticks.contains_key(id)
            {
                return Err(WorldError::DescriptorStorageMissing {
                    component_id: *id,
                    archetype_id,
                });
            }
        }

        let index = archetype.entities.len();
        archetype.entities.push(entity);

        // Step 4: Push each raw byte payload and a fresh change tick into
        // the entity's new row. The lookups cannot fail after the pass above,
        // but they report rather than panic so a future refactor that drops
        // the pre-flight check degrades into an error instead of unwinding.
        for (id, bytes) in components {
            match archetype.component_storages.get_mut(*id) {
                Some(storage) => storage.push_bytes(bytes)?,
                None => {
                    return Err(WorldError::DescriptorStorageMissing {
                        component_id: *id,
                        archetype_id,
                    })
                }
            }
            match archetype.component_ticks.get_mut(id) {
                Some(ticks) => ticks.push(ComponentTicks::new(current_tick)),
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

    /// Read one runtime-defined component as its raw manifest bytes.
    pub fn descriptor_component_bytes(
        &self,
        entity: Entity,
        component_id: ComponentId,
    ) -> Option<&[u8]> {
        let location = self.entity_locations.get(&entity)?;
        self.archetypes
            .get(&location.archetype_id)?
            .component_storages
            .get(component_id)?
            .bytes(location.index_in_archetype)
    }

    /// Register a script component type with the World
    ///
    /// Script components have an update() method that gets called by update_scripts().
    /// This must be called for each script component type before it can be used.
    pub fn register_script_component<T>(&mut self)
    where
        T: ScriptComponent + Clone,
    {
        let _zone = crate::profile_scope!(
            "register script component",
            [(
                "Script component type being registered: {}",
                std::any::type_name::<T>()
            )]
        );
        // First register as a normal component
        self.register_component::<T>();

        // Then track it as a script component
        let component_id = ComponentId::of::<T>();
        if let Some(bit) = self.component_registry.get_bit(&component_id) {
            self.script_components.push((component_id, bit));

            // Register updater callback for this script component.
            // Uses a non-capturing closure coerced to a function pointer
            // so that no state (especially no raw pointer) is captured.
            // The raw pointers are passed fresh by `update_scripts` on
            // every invocation.
            self.script_updaters.insert(
                component_id,
                (|storage: &mut ComponentColumns,
                  index: usize,
                  entity: Entity,
                  world_ptr: *mut World,
                  commands_ptr: *mut CommandQueue| {
                    // Get mutable reference to the component
                    let component = storage.column_of_mut::<T>().get_mut::<T>(index);
                    // SAFETY: `world_ptr` and `commands_ptr` are derived from
                    // `&mut World` / `&mut CommandQueue` that are valid for the
                    // entire duration of `update_scripts`, which is the sole
                    // caller of every stored updater. The function-pointer
                    // representation prevents these pointers from being cached
                    // across calls.
                    unsafe {
                        let mut script_context =
                            ScriptContext::new(&mut *world_ptr, &mut *commands_ptr, entity);
                        component.update(&mut script_context);
                    }
                }) as ScriptUpdater,
            );
        }
    }

    /// Update all script components
    ///
    /// Calls update() on every script component in the world.
    /// Scripts receive a `ScriptContext` with:
    /// - Read-only world access for queries
    /// - Deferred command queue for structural changes
    ///
    /// This ensures all structural changes (add/remove component, destroy entity)
    /// are automatically deferred, preventing use-after-free bugs.
    pub(crate) fn update_scripts(&mut self, commands: &mut CommandQueue) {
        let _zone = crate::profile_scope!(
            "update scripts",
            [(
                "Script component types in world: {}",
                self.script_components.len()
            )]
        );
        // Step 1: Reserve the per-frame work list and capture raw pointers to
        // self and the command queue before any field borrows on self.
        let total_entities = self.entity_locations.len();
        let mut entities_to_update: Vec<(Entity, ArchetypeId, usize)> =
            Vec::with_capacity(total_entities);

        // Take raw pointers once, BEFORE any field borrows on self.
        let world_ptr = self as *mut World;
        let commands_ptr = commands as *mut CommandQueue;

        // Step 2: For each script component type, gather every entity that
        // carries it.
        for &(component_id, comp_bit) in &self.script_components {
            // Get the updater for this component type.
            // Function pointers are Copy - no allocation here.
            let updater = match self.script_updaters.get(&component_id) {
                Some(&u) => u,
                None => continue,
            };

            // Collect entities that have this script component
            for (archetype_id, archetype) in &self.archetypes {
                // Check if this archetype has the script component using bitmask
                let mut mask = ComponentMask::empty();
                mask.set(comp_bit);

                if archetype.matches_mask(&mask) {
                    // Collect all entities in this archetype
                    for (index, &entity) in archetype.entities.iter().enumerate() {
                        entities_to_update.push((entity, *archetype_id, index));
                    }
                }
            }

            // Step 3: Sort the gathered entities for deterministic order
            // across runs, then dispatch each one to its updater with the
            // captured raw pointers.
            entities_to_update.sort_by_key(|(_, aid, idx)| (*aid, *idx));

            for (entity, archetype_id, index) in entities_to_update.drain(..) {
                if let Some(archetype) = self.archetypes.get_mut(&archetype_id) {
                    // Call the updater with mutable storage access
                    updater(
                        &mut archetype.component_storages,
                        index,
                        entity,
                        world_ptr,
                        commands_ptr,
                    );
                }
            }
        }
    }

    // ----------------------------------------------------------------------------
    // Change Detection - "what changed since my system last ran?"
    // ----------------------------------------------------------------------------
    //
    // Every component and resource stores two tick values: `added` and `changed`.
    // The world bumps a global tick counter each frame.  When you write to a
    // component (through `&mut T` in a query, via `Mut<T>`), its `changed`
    // tick is set to the current world tick.
    //
    // Filters like `Changed<T>` and `Added<T>` compare each entity's ticks
    // against a *baseline* - the tick at which the calling system last ran.
    // If a component's `changed` tick is newer than that baseline, the entity
    // is yielded.  This is how "only process entities that were modified since
    // I last looked" works without any manual dirty flags.
    //
    // The baseline comes from one of two places:
    //
    //   SEQUENTIAL mode → world.system_last_run  (one shared field)
    //   PARALLEL  mode → per-thread override      (no sharing, no races)
    //
    // In parallel mode the Engine sets a thread-local override before each
    // system runs, so every thread sees the correct baseline for its own
    // system without touching shared state.

    /// Read the current world tick without modifying it.
    #[inline]
    pub fn change_tick(&self) -> Tick {
        Tick::new(self.change_tick)
    }

    /// Bump the world tick and return the new value.
    ///
    /// Called by the [`Engine`](crate::engine::Engine) once per frame and
    /// by mutable queries when they begin iteration so that mutations
    /// performed during the same frame can still be distinguished by tick.
    ///
    /// A parallel batch runs every one of its systems against the same world,
    /// so bumping here would be an unsynchronized read-modify-write. The
    /// engine therefore allocates one tick per batch member on the dispatch
    /// thread and installs it as this thread's override; inside a system the
    /// reserved value is returned however often it is asked for, and the
    /// shared counter is left untouched.
    #[inline]
    pub fn increment_change_tick(&mut self) -> Tick {
        if let Some(reserved) = per_thread_this_run_tick() {
            return reserved;
        }
        self.change_tick = self.change_tick.wrapping_add(1);
        Tick::new(self.change_tick)
    }

    /// What tick was current when the calling system last ran?
    ///
    /// If a per-thread override is active (parallel execution), that value
    /// wins.  Otherwise fall back to the shared world field (sequential
    /// execution or ad-hoc queries).
    #[inline]
    pub fn system_last_run(&self) -> Tick {
        if let Some(t) = per_thread_last_run_tick() {
            return t;
        }
        Tick::new(self.system_last_run)
    }

    /// Set the world-level baseline directly.  Prefer letting the Engine
    /// manage this - this method exists mainly for tests and one-off queries.
    #[inline]
    pub fn set_system_last_run(&mut self, tick: Tick) {
        self.system_last_run = tick.get();
    }
}

impl World {
    // ----------------------------------------------------------------------------
    // Resource Management
    // ----------------------------------------------------------------------------

    /// Insert a resource (singleton data not attached to any entity)
    ///
    /// Resources are global state such as time, input, configuration, etc.
    /// If a resource of this type already exists, it is replaced.
    ///
    /// A shared name claimed by another type is the exception: the claim is
    /// refused, its error recorded for the caller's drain, and **nothing is
    /// stored**. The generation that asked for it is one init away from being
    /// discarded, and storing its value would replace a live resource with a
    /// shape no reader can accept - or, when the layouts happen to agree,
    /// quietly hand one type's bytes to the other.
    pub fn insert_resource<T: Resource>(&mut self, resource: T) {
        let _zone = crate::profile_scope!(
            "insert resource",
            [(
                "Resource type being inserted: {}",
                std::any::type_name::<T>()
            )]
        );
        let id = ResourceId::of::<T>();
        if !self.claim_shared_resource_name::<T>() {
            return;
        }
        let tick = Tick::new(self.change_tick);
        self.note_resource_registration(id);
        self.resources.insert(id, ErasedResource::new(resource));
        self.resource_ticks.insert(id, ComponentTicks::new(tick));
        // Record the table too, so a later reload can re-home this value even
        // if the next generation never inserts it again.
        self.resource_factories
            .insert(id, ErasedResourceOps::of::<T>());
    }

    /// Get immutable reference to a resource
    pub fn get_resource<T: Resource>(&self) -> Option<&T> {
        let _zone = crate::profile_scope!(
            "get resource",
            [(
                "Resource type being accessed (immutable): {}",
                std::any::type_name::<T>()
            )]
        );
        self.resources
            .get(&ResourceId::of::<T>())
            .and_then(ErasedResource::get::<T>)
    }

    /// Get mutable reference to a resource.
    ///
    /// Prefer [`get_resource_mut_tracked`] for system-parameter usage so
    /// that change-detection ticks are automatically bumped on mutation.
    pub fn get_resource_mut<T: Resource>(&mut self) -> Option<&mut T> {
        self.resources
            .get_mut(&ResourceId::of::<T>())
            .and_then(ErasedResource::get_mut::<T>)
    }

    /// Get mutable, change-tracking access to a resource.
    ///
    /// Returns a [`Mut<'_, T>`] that wraps both the resource value and its
    /// [`ComponentTicks`]. Mutating through `DerefMut` automatically bumps
    /// `ticks.changed` to the current world tick, exactly like mutable
    /// component queries do.
    ///
    /// This is used by [`ResMut`](crate::query::ResMut) so that systems
    /// can later detect resource changes via tick inspection.
    ///
    /// # Panics (debug only)
    ///
    /// Panics if this resource was already fetched mutably during the
    /// current frame - indicates a scheduler bug where two systems
    /// obtained concurrent `&mut` access to the same resource.
    pub fn get_resource_mut_tracked<T: Resource>(&mut self) -> Option<Mut<'_, T>> {
        let _zone = crate::profile_scope!(
            "get resource mut tracked",
            [(
                "Resource type being accessed (mutable, tracked): {}",
                std::any::type_name::<T>()
            )]
        );
        let id = ResourceId::of::<T>();
        let value: &mut T = self
            .resources
            .get_mut(&id)
            .and_then(ErasedResource::get_mut::<T>)?;
        let ticks: &mut ComponentTicks = self.resource_ticks.get_mut(&id).unwrap_or_else(|| {
            // `insert_resource` writes `resources` and `resource_ticks`
            // together, so a resource present in one and absent from the
            // other is an internal-invariant break. Naming the resource
            // keeps the diagnosis startable.
            panic!(
                "resource {id:?} exists in resources but has no change-tick \
                     column; resource_ticks has fallen out of sync with resources"
            )
        });
        let this_run = Tick::new(self.change_tick);
        Some(Mut::new(value, ticks, this_run))
    }

    /// Declare that this artifact owns the resource type `T`, without
    /// inserting a value.
    ///
    /// The resource equivalent of `register_component`, and needed for the
    /// same reason: a resource's stored drop function points into the artifact
    /// that inserted the value, and a reloaded generation that wants to keep
    /// the existing value never calls `insert_resource`, so nothing else would
    /// contribute a table pointing at code that is still mapped.
    ///
    /// Call it from a module's `init` for every resource type the module owns.
    /// Idempotent - a reload re-runs `init`, which is the point.
    ///
    /// A refused claim records its error and leaves the stored table alone, so
    /// a generation that disagrees about a name's owner or shape cannot
    /// re-point the live resource at its own code before its init fails.
    pub fn register_resource<T: Resource>(&mut self) {
        self.register_resource_claiming::<T>();
    }

    /// [`Self::register_resource`], reporting whether the claim was accepted.
    ///
    /// The public call swallows a refusal because its callers have nothing to
    /// do with one - the error is recorded and the init that raised it fails.
    /// The persistable registration does have something to do with it: a
    /// refused generation must not go on to install a serializer that reads the
    /// live value through code that is one init away from being discarded.
    pub(crate) fn register_resource_claiming<T: Resource>(&mut self) -> bool {
        let id = ResourceId::of::<T>();
        if !self.claim_shared_resource_name::<T>() {
            return false;
        }
        self.resource_factories
            .insert(id, ErasedResourceOps::of::<T>());
        self.note_resource_registration(id);
        true
    }

    /// The declared field layout of a foreign resource, if one was recorded.
    ///
    /// The resource twin of [`Self::component_field_layout`]. `None` means the
    /// resource was registered without a layout, which is the ordinary state
    /// for a Rust resource: its fields are a Rust type's, not a manifest's.
    #[must_use]
    pub fn resource_field_layout(
        &self,
        resource_id: ResourceId,
    ) -> Option<&[crate::component_registry::ComponentFieldDescriptor]> {
        self.resource_field_layouts
            .get(&resource_id)
            .map(ComponentFieldLayout::fields)
    }

    /// Record the declared field layout of a foreign resource.
    ///
    /// Overwrites any previous layout for the same id, so a manifest reload
    /// replaces rather than accumulates - the same rule
    /// [`Self::register_component_descriptor_with_layout`] follows.
    ///
    /// **A relayout clears the stored layout**, so this has to be called again
    /// after one. That is deliberate: serving the previous generation's offsets
    /// over the migrated bytes would be worse than serving nothing, and a
    /// caller that forgets loses inspectability rather than correctness.
    ///
    /// # Errors
    ///
    /// Returns [`ComponentFieldError::UnsupportedField`](crate::component_field::ComponentFieldError::UnsupportedField)
    /// for a descriptor that reaches past the resource's registered size or
    /// names a container tag. A foreign resource's bytes are as opaque as a
    /// descriptor row, so it needs both checks for the same reason: the bound
    /// keeps a reader inside the value, and the tag check keeps it from
    /// following a `(pointer, length)` pair the bytes never held.
    pub fn register_resource_field_layout(
        &mut self,
        resource_id: ResourceId,
        fields: Vec<crate::component_registry::ComponentFieldDescriptor>,
    ) -> Result<(), crate::component_field::ComponentFieldError> {
        let resource_size = self
            .resource_factories
            .get(&resource_id)
            .map(|ops| ops.size);
        for field in &fields {
            let Some(end) = field.offset.checked_add(field.size) else {
                return Err(
                    crate::component_field::ComponentFieldError::UnsupportedField {
                        field: field.name.to_string(),
                        reason: "the field's range overflows",
                    },
                );
            };
            if resource_size.is_some_and(|size| end > size) {
                return Err(
                    crate::component_field::ComponentFieldError::UnsupportedField {
                        field: field.name.to_string(),
                        reason: "the field extends past the resource's registered size",
                    },
                );
            }
            if crate::component_field::is_container_tag(field.type_tag) {
                return Err(crate::component_field::ComponentFieldError::UnsupportedField {
                    field: field.name.to_string(),
                    reason: "a container tag is read as a native pointer and length, which a foreign resource's bytes do not hold",
                });
            }
        }
        self.resource_field_layouts
            .insert(resource_id, ComponentFieldLayout::from_owned(fields));
        Ok(())
    }

    /// Declare a resource defined by another language, without storing a value.
    ///
    /// The foreign-language counterpart of [`Self::register_resource`], for a
    /// resource type this process has no Rust definition of. Identity is the
    /// declared name, hashed exactly as [`Resource::shared_name`] hashes a Rust
    /// type's, so a managed declaration and a Rust type that write down the same
    /// string are one resource - the scheduler included, because resource access
    /// sets are id sets.
    ///
    /// The payload is blittable bytes: `size` and `align` describe it, and
    /// [`Self::insert_foreign_resource_bytes`] stores it. Nothing here runs a
    /// destructor, so a foreign value outlives whatever assembly produced it.
    ///
    /// `declaring_type` is the declaring language's own name for the type (the
    /// managed full name, for the C# path). It is recorded, and compared only
    /// against another Rust declaration - see [`Self::claim_shared_resource`].
    ///
    /// # Errors
    ///
    /// [`WorldError::ForeignResourceNameEmpty`] for an empty name,
    /// [`WorldError::ForeignResourceLayoutInvalid`] for a layout that cannot
    /// describe an allocation, [`WorldError::SharedResourceNameConflict`] when
    /// two Rust types disagree about the name,
    /// [`WorldError::SharedResourceLayoutMismatch`] when the declared layout
    /// disagrees with the one already claimed, and
    /// [`WorldError::ForeignResourceHoldsRustValue`] when a Rust value is
    /// stored under the id.
    pub fn register_foreign_resource(
        &mut self,
        name: &str,
        declaring_type: &str,
        size: usize,
        align: usize,
        schema_hash: u64,
    ) -> Result<ResourceId, WorldError> {
        if name.is_empty() {
            return Err(WorldError::ForeignResourceNameEmpty);
        }
        if !is_valid_foreign_layout(size, align) {
            return Err(WorldError::ForeignResourceLayoutInvalid { size, align });
        }
        let id = ResourceId::Shared(crate::component::shared_component_identity(name));
        if let Some(error) = self.claim_shared_resource(
            id,
            name,
            declaring_type,
            size,
            align,
            true,
            Some(schema_hash),
        ) {
            return Err(error);
        }
        // A stored Rust value fixes what is under the id - its destructor is
        // bound to its type - so a foreign declaration cannot take the id over.
        // `relayout_foreign_resource` refuses the same state; refusing here too
        // means the disagreement never reaches `rehome_resources`.
        if self
            .resources
            .get(&id)
            .is_some_and(|value| !value.is_foreign())
        {
            return Err(WorldError::ForeignResourceHoldsRustValue { id });
        }
        self.resource_factories
            .insert(id, ErasedResourceOps::foreign(size, align, schema_hash));
        self.note_resource_registration(id);
        Ok(id)
    }

    /// Store a value for a shared resource, from another language's bytes.
    ///
    /// Replaces a foreign payload under the id; the change tick is stamped,
    /// because a value appearing is a change. A Rust value is never replaced:
    /// its destructor is bound to its type, so admitting foreign bytes would
    /// drop it through code that never allocated it. That refusal is
    /// [`WorldError::ForeignResourceFactoryIsNative`].
    ///
    /// The id has to name a registered *shared* resource, one whose identity is
    /// a declared name. A Rust type's private resource is identified by its
    /// `TypeId`, which no other language can name, so there is nothing here to
    /// address.
    ///
    /// # Errors
    ///
    /// [`WorldError::SharedResourceNotRegistered`] when the id is not a
    /// registered shared resource, [`WorldError::ForeignResourceFactoryIsNative`]
    /// when the id's table is a Rust type's, and
    /// [`WorldError::ForeignResourceBytesMismatch`] when the payload is not
    /// exactly the registered size.
    pub fn insert_foreign_resource_bytes(
        &mut self,
        id: ResourceId,
        bytes: &[u8],
    ) -> Result<(), WorldError> {
        let Some(ops) = self.shared_resource_ops(id) else {
            return Err(WorldError::SharedResourceNotRegistered { id });
        };
        if !ops.foreign {
            return Err(WorldError::ForeignResourceFactoryIsNative { id });
        }
        if bytes.len() != ops.size {
            return Err(WorldError::ForeignResourceBytesMismatch {
                id,
                expected: ops.size,
                actual: bytes.len(),
            });
        }
        let tick = Tick::new(self.change_tick);
        self.resources.insert(
            id,
            ErasedResource::new_foreign(bytes, ops.align, ops.schema_hash.unwrap_or(0)),
        );
        self.resource_ticks.insert(id, ComponentTicks::new(tick));
        self.note_resource_registration(id);
        Ok(())
    }

    /// The stored bytes of a shared resource, for another language to read.
    ///
    /// `None` when the id is not a registered shared resource, when nothing is
    /// stored under it yet, or when the stored value is a Rust one: bytes are
    /// for foreign payloads, whose layout is the whole of what they are.
    pub fn foreign_resource_bytes(&self, id: ResourceId) -> Option<&[u8]> {
        self.foreign_resource_ops(id)?;
        self.resources
            .get(&id)
            .filter(|resource| resource.is_foreign())
            .map(ErasedResource::bytes)
    }

    /// The stored bytes of a shared resource, mutably, with its ticks.
    ///
    /// The `changed` tick moves as the view is handed out rather than on the
    /// first write: a caller holding the bytes has no wrapper that could notice
    /// a mutation, so the conservative reading is that the value changed when
    /// the borrow was taken.
    ///
    /// A caller writing here is responsible for writing a value the stored type
    /// accepts, exactly as a generated mirror is for the component rows it
    /// writes in place.
    ///
    /// A Rust value refuses, as it does for [`Self::foreign_resource_bytes`],
    /// and the refusal costs nothing: the `changed` tick is not stamped for a
    /// view that is never handed out.
    pub fn foreign_resource_bytes_mut(
        &mut self,
        id: ResourceId,
    ) -> Option<(&mut [u8], &mut ComponentTicks)> {
        self.foreign_resource_ops(id)?;
        if !self.resources.get(&id)?.is_foreign() {
            return None;
        }
        let changed = Tick::new(self.change_tick);
        let ticks = self.resource_ticks.get_mut(&id)?;
        ticks.set_changed(changed);
        let bytes = self.resources.get_mut(&id)?.bytes_mut();
        Some((bytes, ticks))
    }

    /// The declared layout of a foreign resource: size, alignment, schema hash.
    ///
    /// What a host compares against the next generation's manifest before it
    /// decides whether a reload needs a migration at all.
    pub fn foreign_resource_layout(&self, id: ResourceId) -> Option<(usize, usize, u64)> {
        let ops = self.resource_factories.get(&id).copied()?;
        if !ops.foreign {
            return None;
        }
        Some((ops.size, ops.align, ops.schema_hash?))
    }

    /// Replace a foreign resource's declared layout, migrating its value.
    ///
    /// The resource twin of [`Self::relayout_descriptor_component`], and much
    /// smaller for the same reason a resource is smaller than a column: one
    /// value, no archetypes, no rows to keep in step. `plan` comes from the same
    /// two field lists the component path uses
    /// ([`FieldPlan::between`]), and anything it does not cover is left
    /// zero.
    ///
    /// Only a foreign payload is rewritten. A Rust value stored under the
    /// declaration refuses: its shape *is* its type, so a declaration that
    /// disagrees with it is a conflict to report rather than a migration to
    /// perform.
    ///
    /// Returns 1 when a value was rewritten and 0 when only the declaration
    /// moved, which is the case for a resource nobody has stored yet.
    ///
    /// # Errors
    ///
    /// [`WorldError::ForeignResourceNotRegistered`] when the id is not a
    /// registered foreign declaration,
    /// [`WorldError::ForeignResourceLayoutInvalid`] for a layout that cannot
    /// describe an allocation,
    /// [`WorldError::ForeignResourcePlanOutOfBounds`] when the plan does not fit
    /// the old or the new payload, and
    /// [`WorldError::ForeignResourceHoldsRustValue`] when a Rust value is stored
    /// under the id.
    pub fn relayout_foreign_resource(
        &mut self,
        id: ResourceId,
        size: usize,
        align: usize,
        schema_hash: u64,
        plan: &FieldPlan,
    ) -> Result<usize, WorldError> {
        if !self
            .resource_factories
            .get(&id)
            .is_some_and(|ops| ops.foreign)
        {
            return Err(WorldError::ForeignResourceNotRegistered { id });
        }
        if !is_valid_foreign_layout(size, align) {
            return Err(WorldError::ForeignResourceLayoutInvalid { size, align });
        }
        if self
            .resources
            .get(&id)
            .is_some_and(|value| !value.is_foreign())
        {
            return Err(WorldError::ForeignResourceHoldsRustValue { id });
        }

        let next = ErasedResourceOps::foreign(size, align, schema_hash);
        // The claim is the second record of this resource's shape, and the one
        // the next declaration is checked against, so it moves with the
        // factory on both paths. Left behind, it made the migrated shape
        // unre-declarable and let a stale declaration revert the factory.
        if let Some(claim) = self.shared_resource_claims.get_mut(&id) {
            claim.size = size;
            claim.align = align;
            claim.schema_hash = Some(schema_hash);
        }
        let Some(value) = self.resources.get_mut(&id) else {
            self.resource_factories.insert(id, next);
            self.resource_field_layouts.remove(&id);
            return Ok(0);
        };
        value
            .migrate_bytes(size, align, plan)
            .map_err(|_error| WorldError::ForeignResourcePlanOutOfBounds { id })?;
        self.resource_factories.insert(id, next);
        // The stored layout described the shape that just moved, so it is
        // dropped rather than left to be served over the migrated bytes. The
        // declarer re-registers it, exactly as it does for a component.
        self.resource_field_layouts.remove(&id);
        Ok(1)
    }

    /// Move a foreign resource's value onto another id and retire the source
    /// declaration.
    ///
    /// The resource twin of [`Self::remap_descriptor_component`] and the
    /// rename half of the managed manifest's resource story: a successor
    /// declaration derives its id from its own name, so the stored value is
    /// reshaped through `plan` - measured from the source's field list to the
    /// successor's - written under the successor, and the source is then
    /// dropped. Any payload already under the successor is replaced; a fresh
    /// declaration's zero seed is the expected case, and the moved value is
    /// the only meaningful one there.
    ///
    /// The source's claims move to the successor wholesale, so the source's
    /// value is released here only when no other subject still declares the
    /// old name, and the successor ends up protected exactly as the source
    /// was.
    ///
    /// Returns 1 when a value was moved and 0 when the source had none stored,
    /// which is the case for a resource nobody has written yet.
    ///
    /// # Errors
    ///
    /// [`WorldError::ForeignResourceRemapSelf`] when both ids are equal,
    /// [`WorldError::ForeignResourceNotRegistered`] when either id is not a
    /// registered foreign declaration, and
    /// [`WorldError::ForeignResourceHoldsRustValue`] when a Rust value is
    /// stored under either id.
    pub fn remap_foreign_resource(
        &mut self,
        old_id: ResourceId,
        new_id: ResourceId,
        plan: &FieldPlan,
    ) -> Result<usize, WorldError> {
        if old_id == new_id {
            return Err(WorldError::ForeignResourceRemapSelf);
        }
        let Some((old_size, _, _)) = self.foreign_resource_layout(old_id) else {
            return Err(WorldError::ForeignResourceNotRegistered { id: old_id });
        };
        let Some((new_size, _, _)) = self.foreign_resource_layout(new_id) else {
            return Err(WorldError::ForeignResourceNotRegistered { id: new_id });
        };
        if self
            .resources
            .get(&old_id)
            .is_some_and(|value| !value.is_foreign())
            || self
                .resources
                .get(&new_id)
                .is_some_and(|value| !value.is_foreign())
        {
            return Err(WorldError::ForeignResourceHoldsRustValue { id: old_id });
        }

        // Step 1: Read and reshape the stored value before anything moves.
        // `FieldPlan::validate` speaks descriptor rows, so the bounds are
        // checked here and reported with the resource's own variant.
        let mut moved = 0;
        if let Some(old_bytes) = self.foreign_resource_bytes(old_id).map(<[u8]>::to_vec) {
            let mut new_bytes = vec![0_u8; new_size];
            for planned in plan.fields() {
                let FieldSource::OldOffset(source_offset) = planned.source else {
                    continue;
                };
                if planned.offset + planned.bytes > new_size
                    || source_offset + planned.bytes > old_size
                {
                    return Err(WorldError::ForeignResourcePlanOutOfBounds { id: old_id });
                }
                new_bytes[planned.offset..planned.offset + planned.bytes]
                    .copy_from_slice(&old_bytes[source_offset..source_offset + planned.bytes]);
            }
            // Step 2: Write it under the successor. The insert stamps the
            // change tick, so a reader observes the move as the change it is.
            self.insert_foreign_resource_bytes(new_id, &new_bytes)?;
            moved = 1;
        }

        // Step 3: Move the claims and drop the source. The claims are taken
        // wholesale rather than released one by one: whoever declared the old
        // name still needs the resource under its new one.
        let claims = self.resource_claim_counts.remove(&old_id).unwrap_or(0);
        if claims > 0 {
            *self.resource_claim_counts.entry(new_id).or_insert(0) += claims;
        }
        self.drop_resources(&[old_id]);
        Ok(moved)
    }

    /// The per-type table of a registered shared resource, if the id names one.
    ///
    /// Shared identity is the gate: a private resource's id is a `TypeId`, so
    /// no other language can name it and no byte view of it exists to hand out.
    /// The `?` consumes the identity itself - absent means private.
    fn shared_resource_ops(&self, id: ResourceId) -> Option<ErasedResourceOps> {
        id.shared_identity()?;
        self.resource_factories.get(&id).copied()
    }

    /// The per-type table of a registered shared resource, when that table
    /// belongs to foreign bytes.
    ///
    /// The byte accessors go through this one. `shared_resource_ops` answers
    /// "is this a shared id at all", which the foreign insert needs in order to
    /// tell a private id from one whose table is a Rust type's; a byte view, by
    /// contrast, is only meaningful for a payload with no Rust type anywhere,
    /// so both the table and the box have to say so.
    fn foreign_resource_ops(&self, id: ResourceId) -> Option<ErasedResourceOps> {
        self.shared_resource_ops(id).filter(|ops| ops.foreign)
    }

    /// Stamp one resource registration, keeping one entry per id.
    ///
    /// The stamp is the id's most recent registration, not its first. The host
    /// diffs [`Self::resource_ids_registered_since`] after an artifact's `init`
    /// to decide which resources that generation still claims, and a
    /// generation re-claims its resources by re-registering them - the
    /// documented replace path of `insert_resource`. Keeping only the first
    /// stamp made every reload read its own resources as retired and drop them
    /// out from under the systems that were still running. The map stays
    /// proportional to distinct resources, and the diff reports each id once.
    fn note_resource_registration(&mut self, id: ResourceId) {
        self.resource_registration_stamps
            .insert(id, self.resource_registration_sequence);
        self.resource_registration_sequence = self.resource_registration_sequence.wrapping_add(1);
    }

    /// Sequence marker to capture before an artifact's `init`, so the resource
    /// types that `init` registers can be enumerated afterwards.
    ///
    /// Mirrors [`Self::component_registration_sequence`].
    pub fn resource_registration_sequence(&self) -> u64 {
        self.resource_registration_sequence
    }

    /// Resource ids registered at or after `sequence`.
    ///
    /// The comparison is `>=`: a registration reads the current value and then
    /// increments it, so one happening immediately after the capture carries
    /// exactly the captured value.
    pub fn resource_ids_registered_since(&self, sequence: u64) -> Vec<ResourceId> {
        let mut ids: Vec<ResourceId> = self
            .resource_registration_stamps
            .iter()
            .filter(|(_, stamped)| **stamped >= sequence)
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Names declared by the shared resources this world holds a claim for,
    /// sorted and deduplicated.
    ///
    /// The resource counterpart of the component registry's shared-identity
    /// list, and what the ECS report prints. A shared resource's whole point is
    /// that no `TypeId` names it, so the declared name is all there is to
    /// report.
    #[must_use]
    pub fn shared_resource_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .shared_resource_claims
            .values()
            .map(|claim| claim.shared_name.clone())
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    /// Check and record which type owns a shared resource name.
    ///
    /// Returns whether the caller may go on to register: `true` for an ordinary
    /// resource, whose id *is* its `TypeId`, so a second claim on one id is
    /// necessarily the same type, and for a shared claim that is accepted;
    /// `false` when the claim was refused and its error recorded.
    ///
    /// A refusal has to stop the caller from touching what is stored, not just
    /// fail the init later: the reload transaction rolls the generation back by
    /// re-running the previous `init`, which re-registers what it owned rather
    /// than restoring values, so a value written under a refused claim would
    /// outlive the generation that wrote it - in a shape nothing can read.
    ///
    /// For a shared resource the id is derived from a written-down name, so
    /// two unrelated types can reach it. The discriminator is the type's own
    /// name - two copies of one type agree on it, two different types do not -
    /// and only the final path segment is compared, because the module path
    /// differs between artifacts while the type's name does not. This is the
    /// same rule `ComponentRegistry` applies to shared components.
    fn claim_shared_resource_name<T: Resource>(&mut self) -> bool {
        let Some(shared_name) = T::shared_name() else {
            return true;
        };
        match self.claim_shared_resource(
            ResourceId::of::<T>(),
            shared_name,
            crate::component::ComponentRegistry::declaring_type_name::<T>(),
            std::mem::size_of::<T>(),
            std::mem::align_of::<T>(),
            false,
            <T as Resource>::shared_schema_hash(),
        ) {
            None => true,
            Some(error) => {
                // Recorded here rather than in the core: the Rust path is the
                // one whose refusal has to fail the init that raised it. A
                // foreign declaration's caller gets the error back and acts on
                // it itself.
                self.record_registration_error(error);
                false
            }
        }
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
    #[allow(clippy::too_many_arguments)]
    fn claim_shared_resource(
        &mut self,
        id: ResourceId,
        shared_name: &str,
        declaring_type: &str,
        size: usize,
        align: usize,
        foreign: bool,
        schema_hash: Option<u64>,
    ) -> Option<WorldError> {
        let incoming = SharedResourceClaim {
            shared_name: shared_name.to_string(),
            declaring_type: declaring_type.to_string(),
            foreign,
            size,
            align,
            schema_hash,
        };
        if let Some(existing) = self.shared_resource_claims.get(&id) {
            // The id is a hash of the name, so two different names sharing it
            // are an identity collision: one slot would answer to both. The
            // recorded string is what tells the two apart.
            if existing.shared_name != shared_name {
                let error = WorldError::SharedResourceIdentityCollision {
                    shared_name: shared_name.to_string(),
                    existing_name: existing.shared_name.clone(),
                };
                error!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    shared_name = %shared_name,
                    existing_name = %existing.shared_name,
                    error = %error,
                    "two shared resource names hash to one identity"
                );
                return Some(error);
            }
            if !existing.foreign
                && !incoming.foreign
                && existing.declaring_type != incoming.declaring_type
            {
                // Logged as well as returned, exactly as `register_component`
                // logs its own failures: the log line is what names the cause
                // to whoever reads the host output.
                let error = WorldError::SharedResourceNameConflict {
                    shared_name: shared_name.to_string(),
                    existing_type: existing.declaring_type.clone(),
                    incoming_type: incoming.declaring_type.clone(),
                };
                error!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    shared_name = %shared_name,
                    existing_type = %existing.declaring_type,
                    incoming_type = %incoming.declaring_type,
                    error = %error,
                    "shared resource name claimed by two different types"
                );
                return Some(error);
            }
            if existing.size != incoming.size || existing.align != incoming.align {
                let error = WorldError::SharedResourceLayoutMismatch {
                    shared_name: shared_name.to_string(),
                    existing_size: existing.size,
                    existing_align: existing.align,
                    incoming_size: incoming.size,
                    incoming_align: incoming.align,
                };
                error!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    shared_name = %shared_name,
                    existing_size = existing.size,
                    existing_align = existing.align,
                    incoming_size = incoming.size,
                    incoming_align = incoming.align,
                    error = %error,
                    "shared resource registered with two different layouts"
                );
                return Some(error);
            }
            // Both sides carrying a hash means both declared a field shape, and
            // equal size/align is exactly the case a hash catches: {u32, u32}
            // and {f32, f32} agree on layout and not on fields.
            if let (Some(existing_hash), Some(incoming_hash)) =
                (existing.schema_hash, incoming.schema_hash)
            {
                if existing_hash != incoming_hash {
                    let error = WorldError::SharedResourceSchemaMismatch {
                        shared_name: shared_name.to_string(),
                        existing_hash,
                        incoming_hash,
                    };
                    error!(
                        target: pill_core::telemetry::telemetry_target::ECS,
                        shared_name = %shared_name,
                        error = %error,
                        "shared resource registered with two different field shapes"
                    );
                    return Some(error);
                }
            }
        }
        match self.shared_resource_claims.get(&id) {
            None => {
                self.shared_resource_claims.insert(id, incoming);
            }
            // A foreign declaration does not overwrite the record: keeping a
            // Rust declarer's name is what lets a later Rust arrival still be
            // compared against one. A Rust arrival to a foreign claim does take
            // it, because from then on there is a Rust name worth comparing.
            Some(existing) if existing.foreign && !incoming.foreign => {
                self.shared_resource_claims.insert(id, incoming);
            }
            Some(_) => {}
        }
        None
    }

    /// Re-home every stored resource's per-type function table.
    ///
    /// The twin of [`Self::rehome_native_columns`], called at the same point
    /// in the reload transaction and for the same reason: a resource holds a
    /// drop function belonging to whichever artifact inserted it, and that
    /// artifact's image is about to be evicted from the reload graveyard.
    /// Refreshing from the latest registered table, while every image is still
    /// mapped, keeps the drop valid afterwards.
    ///
    /// A resource whose type no artifact has registered in this generation is
    /// left alone - there is nothing newer to point it at. That case is a
    /// retired owner's resource, and [`Self::drop_resources`] is what releases
    /// it, while the retiring image is still mapped.
    ///
    /// A foreign box is left alone as well, for a different reason: its table
    /// drops nothing and points at this crate's code, which outlives every
    /// artifact. Refreshing it from the factories would let a Rust type that
    /// shares the name hand its own destructor to bytes it does not own.
    ///
    /// A table that claims foreign ownership over a Rust value is left alone
    /// too. Registration paths refuse that state, but a stale table could in
    /// principle reach it, and the table would then drop nothing: the value
    /// would leak silently. Keeping its own destructor is the lesser cost, and
    /// the disagreement is reported rather than swallowed.
    pub fn rehome_resources(&mut self) {
        for (id, resource) in &mut self.resources {
            if resource.is_foreign() {
                continue;
            }
            if let Some(&ops) = self.resource_factories.get(id) {
                if ops.foreign {
                    error!(
                        target: pill_core::telemetry::telemetry_target::ECS,
                        resource = ?id,
                        "a Rust resource is declared foreign; keeping its own drop"
                    );
                    continue;
                }
                resource.refresh_ops(ops);
            }
        }
    }

    /// Drop the named resources, releasing their values.
    ///
    /// The resource twin of
    /// [`drop_forgotten_components`](Self::drop_forgotten_components), and it
    /// carries the same timing requirement: **call it while the artifact that
    /// inserted the value is still mapped.** A resource holds a drop function
    /// belonging to that artifact, so dropping it after the image is evicted
    /// calls through a dangling pointer.
    ///
    /// The host uses it when a reloaded module stops registering a resource
    /// type it previously owned - compare
    /// [`resource_ids_registered_since`](Self::resource_ids_registered_since)
    /// across the two generations to find those ids.
    ///
    /// An id another subject still claims through
    /// [`retain_resource_claims`](Self::retain_resource_claims) is skipped: the
    /// value's drop function belongs to the generation that inserted it, and
    /// the subject retiring now is not necessarily that one.
    ///
    /// Returns how many were actually present and dropped.
    pub fn drop_resources(&mut self, ids: &[ResourceId]) -> usize {
        let mut dropped = 0;
        for id in ids {
            // Another subject's claim keeps the value alive.
            if self
                .resource_claim_counts
                .get(id)
                .is_some_and(|count| *count > 0)
            {
                continue;
            }
            if self.resources.remove(id).is_some() {
                dropped += 1;
            }
            self.resource_ticks.remove(id);
            self.resource_factories.remove(id);
            self.resource_field_layouts.remove(id);
            self.shared_resource_claims.remove(id);
            // The id is unregistered now, so a later registration is a fresh
            // one and has to be visible to the next "registered since" diff.
            self.resource_registration_stamps.remove(id);
        }
        dropped
    }

    /// Record one claim per id on a resource this subject registered.
    ///
    /// The count is what [`drop_resources`](Self::drop_resources) consults
    /// before releasing an id one subject retired: a shared name can be
    /// registered from several subjects, and only the last of them to let go
    /// should destroy the value.
    pub fn retain_resource_claims(&mut self, ids: &[ResourceId]) {
        for id in ids {
            *self.resource_claim_counts.entry(*id).or_insert(0) += 1;
        }
    }

    /// Drop one claim per id, releasing a value once its last claim goes.
    ///
    /// A claim an id never had is a no-op, and a release that takes the count
    /// to zero removes the entry, so a later `retain` starts a fresh count.
    pub fn release_resource_claims(&mut self, ids: &[ResourceId]) {
        for id in ids {
            if let Some(count) = self.resource_claim_counts.get_mut(id) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.resource_claim_counts.remove(id);
                }
            }
        }
    }

    /// Remove a resource and return it if it existed.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::SharedResourceHoldsAnotherType`] when the id holds
    /// a value this `T` cannot be read as - the case a shared, name-derived id
    /// makes reachable. The take is attempted first and the box is put back on
    /// refusal, so nothing about the stored resource is touched: not its value,
    /// its ticks, its factory or its claim.
    pub fn remove_resource<T: Resource>(&mut self) -> Result<Option<T>, WorldError> {
        let id = ResourceId::of::<T>();
        let Some(erased) = self.resources.remove(&id) else {
            return Ok(None);
        };
        let value = match erased.take::<T>() {
            Ok(value) => value,
            Err(erased) => {
                // The comment above used to promise this and the code did the
                // opposite: `and_then(..ok())` dropped the returned box, taking
                // the value, its ticks, its factory and its claim with it.
                self.resources.insert(id, erased);
                return Err(WorldError::SharedResourceHoldsAnotherType {
                    id,
                    requested_type: std::any::type_name::<T>(),
                });
            }
        };
        self.resource_ticks.remove(&id);
        self.resource_factories.remove(&id);
        self.shared_resource_claims.remove(&id);
        self.resource_registration_stamps.remove(&id);
        Ok(Some(value))
    }

    /// Check whether a resource exists *and* can be read as `T`.
    ///
    /// The same question [`Self::get_resource`] answers: for a shared id a
    /// differently-shaped value under the name is not a `T`, so existence and
    /// readability agree instead of one reporting the id and the other the
    /// readable shape. [`Self::resource_holder`] answers the shape question for
    /// a caller that needs to know what is stored.
    #[must_use]
    pub fn has_resource<T: Resource>(&self) -> bool {
        self.resources
            .get(&ResourceId::of::<T>())
            .is_some_and(|erased| erased.holds::<T>())
    }

    /// The stored box under `T`'s resource id, whatever shape it holds.
    ///
    /// For a caller whose id exists but whose type does not read: the box
    /// answers `holds::<T>()` and `has_shared_identity()`. `None` means the id
    /// has no resource at all.
    #[must_use]
    pub fn resource_holder<T: Resource>(&self) -> Option<&crate::resource::ErasedResource> {
        self.resources.get(&ResourceId::of::<T>())
    }

    /// Debug-only: Clear the set of mutably-borrowed resources.
    ///
    /// Called by [`Engine::process_frame`] at the start of every frame so
    /// that the isolation check only guards against concurrent access
    /// within a single frame.
    /// Take one resource's debug write lock, reporting an overlapping holder.
    ///
    /// Acquired when a [`ResMut`](crate::query::ResMut) is built and released
    /// when it drops, so the lock spans exactly one system's access. Doing it
    /// per `get_mut()` call instead meant a system that took the resource
    /// twice in sequence tripped the assertion, and holding it to the end of
    /// the frame meant the second of two systems writing one resource did -
    /// both while the message blamed concurrency that was not happening.
    ///
    /// `&self`, not `&mut self`: the systems of a parallel batch hold aliasing
    /// `&mut World`, so this must be callable through a shared reference.
    #[cfg(debug_assertions)]
    pub(crate) fn debug_acquire_resource_lock(&self, id: ResourceId) {
        let newly_inserted = self.debug_resource_write_locks.lock().insert(id);
        debug_assert!(
            newly_inserted,
            "Resource {id:?} is already mutably borrowed by another live \
             `ResMut` - possible scheduler bug or concurrent system access"
        );
    }

    /// Release one resource's debug write lock; see
    /// [`Self::debug_acquire_resource_lock`].
    #[cfg(debug_assertions)]
    pub(crate) fn debug_release_resource_lock(&self, id: ResourceId) {
        self.debug_resource_write_locks.lock().remove(&id);
    }

    #[cfg(debug_assertions)]
    pub(crate) fn debug_clear_resource_locks(&mut self) {
        self.debug_resource_write_locks.lock().clear();
    }

    /// Check if an entity exists and is valid (not destroyed/recycled)
    ///
    /// Returns true if the entity exists in the world with the correct generation.
    /// Returns false if the entity was destroyed or if its ID was recycled with a new generation.
    #[must_use]
    pub fn is_entity_valid(&self, entity: Entity) -> bool {
        self.entity_locations.contains_key(&entity)
    }

    /// Get immutable reference to a component on an entity
    ///
    /// Returns None if the entity doesn't exist or doesn't have the component.
    pub fn get_component<T>(&self, entity: Entity) -> Option<&T>
    where
        T: Component,
    {
        let _zone = crate::profile_scope!(
            "get component",
            [(
                "Target entity: {:?}, Component type: {}",
                entity,
                std::any::type_name::<T>()
            )]
        );
        // Get component bit for O(1) archetype check
        let component_id = ComponentId::of::<T>();
        let bit = self.component_registry.get_bit(&component_id)?;

        // Get entity location
        let location = self.entity_locations.get(&entity)?;

        // Get archetype
        let archetype = self.archetypes.get(&location.archetype_id)?;

        // Check if archetype has this component type (O(1) bitmask check)
        if !archetype.has_component_bit(bit) {
            return None;
        }

        // Get component from storage
        Some(
            archetype
                .component_storages
                .column_of::<T>()
                .get::<T>(location.index_in_archetype),
        )
    }

    /// Get mutable reference to a component on an entity
    ///
    /// Returns None if the entity doesn't exist or doesn't have the component.
    pub fn get_component_mut<T>(&mut self, entity: Entity) -> Option<&mut T>
    where
        T: Component,
    {
        let _zone = crate::profile_scope!(
            "get component mut",
            [(
                "Target entity: {:?}, Component type (mutable): {}",
                entity,
                std::any::type_name::<T>()
            )]
        );
        // Get component bit for O(1) archetype check
        let component_id = ComponentId::of::<T>();
        let bit = self.component_registry.get_bit(&component_id)?;

        // Get entity location
        let location = self.entity_locations.get(&entity)?;
        let archetype_id = location.archetype_id;
        let index = location.index_in_archetype;

        // Get archetype
        let archetype = self.archetypes.get_mut(&archetype_id)?;

        // Check if archetype has this component type (O(1) bitmask check)
        if !archetype.has_component_bit(bit) {
            return None;
        }

        // Get component from storage
        Some(
            archetype
                .component_storages
                .column_of_mut::<T>()
                .get_mut::<T>(index),
        )
    }

    /// Get raw mutable pointer to a component on an entity
    ///
    /// This is used by ScriptContext to avoid aliasing issues when a script
    /// accesses components of its own type. By returning a raw pointer instead
    /// of `&mut T`, we opt out of Rust's noalias optimization.
    ///
    /// Returns None if the entity doesn't exist or doesn't have the component.
    pub(crate) fn get_component_ptr_mut<T>(&mut self, entity: Entity) -> Option<*mut T>
    where
        T: Component,
    {
        // Get component bit for O(1) archetype check
        let component_id = ComponentId::of::<T>();
        let bit = self.component_registry.get_bit(&component_id)?;

        // Get entity location
        let location = self.entity_locations.get(&entity)?;
        let archetype_id = location.archetype_id;
        let index = location.index_in_archetype;

        // Get archetype
        let archetype = self.archetypes.get_mut(&archetype_id)?;

        // Check if archetype has this component type (O(1) bitmask check)
        if !archetype.has_component_bit(bit) {
            return None;
        }

        // Get raw pointer to component - avoids creating intermediate &mut
        let storage = archetype.component_storages.column_of_mut::<T>();
        Some(storage.get_mut::<T>(index) as *mut T)
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

        // Step 4: Maintain change-detection ticks - every component_id in the
        // archetype got exactly one push above, so push one fresh tick.
        for &component_id in &archetype.component_types {
            archetype
                .component_ticks
                .entry(component_id)
                .or_default()
                .push(ComponentTicks::new(current_tick));
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

    /// Move an entity to a new archetype, preserving existing components
    ///
    /// This is used when adding/removing components from an existing entity.
    /// The move_fn closure receives:
    /// 1. Old archetype storage (to read existing components)
    /// 2. New archetype storage (to write all components)
    /// 3. Index of the entity in old archetype
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::EntityNotFound`] when the entity has no location
    /// record, [`WorldError::ArchetypeMissing`] when either the source or the
    /// destination archetype is absent from the world, and
    /// [`WorldError::DescriptorStorageMissing`] when a descriptor component named
    /// by an archetype has no storage column — the desync a partially applied
    /// hot reload can leave behind.
    pub(crate) fn move_entity_to_archetype<F>(
        &mut self,
        entity: Entity,
        new_component_ids: Vec<ComponentId>,
        move_fn: F,
    ) -> Result<(), WorldError>
    where
        F: FnOnce(&ComponentColumns, &mut ComponentColumns, usize),
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

        // Step 2b: Verify the destination before anything moves. A descriptor
        // component the destination archetype has no column for is the
        // manifest/storage desync `DescriptorStorageMissing` reports; finding it
        // here - over the same component list the migration loop itself walks,
        // the destination archetype's own - is what keeps a failed migration
        // atomic: the destination would otherwise already hold the entity's row
        // while the source still holds the entity.
        {
            let new_archetype =
                self.archetypes
                    .get(&new_archetype_id)
                    .ok_or(WorldError::ArchetypeMissing {
                        entity,
                        archetype_id: new_archetype_id,
                    })?;
            for &component_id in &new_archetype.component_types {
                if component_id.is_native_storage() {
                    continue;
                }
                if !new_archetype.component_storages.contains(component_id) {
                    return Err(WorldError::DescriptorStorageMissing {
                        component_id,
                        archetype_id: new_archetype_id,
                    });
                }
            }
        }

        // Step 3: Migrate the entity. We need simultaneous access to two
        // archetypes, which the borrow checker cannot express through the
        // HashMap, so take raw pointers to both entries. The early-return
        // above guarantees the two ArchetypeIds differ, so the entries are
        // disjoint allocations; the debug_assert_ne! below re-checks this
        // inside the unsafe block as a second line of defense against a
        // future refactor removing the early-return.
        let old_archetype_ptr =
            self.archetypes
                .get(&old_archetype_id)
                .ok_or(WorldError::ArchetypeMissing {
                    entity,
                    archetype_id: old_archetype_id,
                })? as *const Archetype;
        let new_archetype_ptr =
            self.archetypes
                .get_mut(&new_archetype_id)
                .ok_or(WorldError::ArchetypeMissing {
                    entity,
                    archetype_id: new_archetype_id,
                })? as *mut Archetype;

        // SAFETY: old_archetype_id != new_archetype_id is proven by the
        // early-return above and re-checked below. Different ArchetypeId
        // values map to different HashMap entries, so old_archetype and
        // new_archetype point to non-overlapping allocations, making the
        // simultaneous `&` and `&mut` access sound. Both pointers stay valid
        // for the duration of this block because the archetypes map is not
        // mutated until after the references derived here are dropped.
        unsafe {
            debug_assert_ne!(
                old_archetype_id, new_archetype_id,
                "move_entity_to_archetype: old and new archetype IDs must differ"
            );
            let old_archetype = &*old_archetype_ptr;
            let new_archetype = &mut *new_archetype_ptr;

            // Read component_types via raw pointer - avoids a Vec clone.
            // The block-level SAFETY above establishes that new_archetype_ptr
            // is valid; component_types is only read (never mutated) here.
            let new_component_ids = &(*new_archetype_ptr).component_types;

            let new_index = new_archetype.entities.len();
            new_archetype.entities.push(entity);

            // Call the move function to copy components
            move_fn(
                &old_archetype.component_storages,
                &mut new_archetype.component_storages,
                old_index,
            );

            // Runtime-defined columns participate in every archetype move
            // without requiring a concrete Rust copier function.
            for &component_id in new_component_ids {
                // `move_fn` above copied every column that has a Rust type.
                // Only descriptor columns are left, and before the two maps
                // merged this loop could not reach a native one to begin with.
                if component_id.is_native_storage() {
                    continue;
                }
                let Some(destination) = new_archetype.component_storages.get_mut(component_id)
                else {
                    // Native ids were skipped above, so reaching here means a
                    // descriptor component has no column: a manifest/storage
                    // desync. Fail the migration rather than leave the
                    // destination archetype short a column.
                    return Err(WorldError::DescriptorStorageMissing {
                        component_id,
                        archetype_id: new_archetype_id,
                    });
                };
                if let Some(source) = old_archetype.component_storages.get(component_id) {
                    destination.push_from(source, old_index)?;
                } else {
                    destination.push_zeroed()?;
                }
            }

            // Maintain change-detection ticks: for each component in the
            // destination archetype, either preserve the existing ticks
            // (component carried over) or push fresh ticks for a newly
            // attached component.
            let current_tick = Tick::new(self.change_tick);
            for &component_id in new_component_ids {
                let new_tick =
                    if let Some(old_ticks_vec) = old_archetype.component_ticks.get(&component_id) {
                        // Component carried over from old archetype. `Archetype`
                        // creates tick columns for every component type in
                        // lockstep, so a row missing here is an internal
                        // invariant break; name the entity and archetype so the
                        // report is startable.
                        *old_ticks_vec.get(old_index).unwrap_or_else(|| {
                            panic!(
                                "old ticks vec out of sync with components while migrating \
                                 entity {entity:?} to archetype {new_archetype_id:?}: row \
                                 {old_index} is missing from {} ticks for {component_id:?}",
                                old_ticks_vec.len()
                            )
                        })
                    } else {
                        // Newly added component on this entity.
                        ComponentTicks::new(current_tick)
                    };
                new_archetype
                    .component_ticks
                    .entry(component_id)
                    .or_default()
                    .push(new_tick);
            }

            // Every destination column grew by exactly one row, and so did
            // every tick column. A miss here means the migration left the
            // destination short a row - the desync the pre-flight above exists
            // to prevent.
            debug_assert_eq!(
                new_archetype.entities.len(),
                new_index + 1,
                "the migrated entity is the destination's last row"
            );
            for &component_id in new_component_ids {
                if !component_id.is_native_storage() {
                    if let Some(column) = new_archetype.component_storages.get(component_id) {
                        debug_assert_eq!(
                            column.len(),
                            new_index + 1,
                            "a destination descriptor column did not grow by exactly one row"
                        );
                    }
                }
                if let Some(ticks) = new_archetype.component_ticks.get(&component_id) {
                    debug_assert_eq!(
                        ticks.len(),
                        new_index + 1,
                        "a destination tick column did not grow by exactly one row"
                    );
                }
            }

            // Update entity location
            self.entity_locations.insert(
                entity,
                EntityLocation {
                    archetype_id: new_archetype_id,
                    index_in_archetype: new_index,
                },
            );
        }

        // Step 4: Remove the entity from the old archetype with swap_remove
        // for O(1) removal, keeping every column in lockstep.
        let Some(old_archetype) = self.archetypes.get_mut(&old_archetype_id) else {
            // The source archetype was resolved at the top of this function
            // and nothing here removes archetypes, so a miss is an internal
            // break. Report it with the same vocabulary as the migration.
            return Err(WorldError::ArchetypeMissing {
                entity,
                archetype_id: old_archetype_id,
            });
        };

        if old_index < old_archetype.entities.len() {
            old_archetype.entities.swap_remove(old_index);

            // Update the location of the entity that was swapped (if any)
            if old_index < old_archetype.entities.len() {
                let swapped_entity = old_archetype.entities[old_index];
                if let Some(swapped_location) = self.entity_locations.get_mut(&swapped_entity) {
                    swapped_location.index_in_archetype = old_index;
                }
            }

            // Also swap_remove from all component storages to keep them in sync
            for &component_id in &old_archetype.component_types {
                match component_id.is_native_storage() {
                    true => {
                        if let Some(storage) =
                            old_archetype.component_storages.get_mut(component_id)
                        {
                            storage.swap_remove_discard(old_index);
                        }
                    }
                    false => {
                        let Some(column) = old_archetype.component_storages.get_mut(component_id)
                        else {
                            // A descriptor component without a column is the
                            // manifest/storage desync this function already
                            // reports during migration; surface it here too
                            // instead of panicking mid-frame.
                            return Err(WorldError::DescriptorStorageMissing {
                                component_id,
                                archetype_id: old_archetype_id,
                            });
                        };
                        column.swap_remove_discard(old_index);
                    }
                }
                // Keep change-detection ticks in lockstep with storage.
                if let Some(ticks) = old_archetype.component_ticks.get_mut(&component_id) {
                    if old_index < ticks.len() {
                        ticks.swap_remove(old_index);
                    }
                }
            }
        }

        // Step 5: If the old archetype is now empty, remove it entirely to
        // prevent memory leaks.
        if old_archetype.entities.is_empty() {
            self.archetypes.remove(&old_archetype_id);
            self.archetype_generation = self.archetype_generation.wrapping_add(1);
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

            // Also swap_remove from all component storages to keep them in sync.
            // component_types, component_ticks, and component_storages are separate
            // fields - Rust's split-borrow allows &component_types alongside mutable
            // access to the other fields, so no clone is needed.
            let component_type_ids: &[ComponentId] = &archetype.component_types;
            for component_id in component_type_ids {
                if let Some(ticks) = archetype.component_ticks.get_mut(component_id) {
                    if old_index < ticks.len() {
                        ticks.swap_remove(old_index);
                    }
                }
            }
            for component_id in component_type_ids {
                match component_id.is_native_storage() {
                    true => {
                        if let Some(storage) = archetype.component_storages.get_mut(*component_id) {
                            storage.swap_remove_discard(old_index);
                        }
                    }
                    false => {
                        if let Some(column) = archetype.component_storages.get_mut(*component_id) {
                            column.swap_remove_discard(old_index);
                        } else {
                            // A descriptor component with no column has no data to
                            // remove, so skipping is safe. The missing column is
                            // the manifest/storage desync reported by
                            // `WorldError::DescriptorStorageMissing`; report rather
                            // than panic, because this runs inside
                            // `process_frame` for managed projects.
                            warn!(
                                target: pill_core::telemetry::telemetry_target::ECS,
                                component_id = ?component_id,
                                archetype_id = ?archetype.id,
                                "destroy_entity: descriptor component has no storage column; skipping"
                            );
                        }
                    }
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

    /// Remove a component from an entity, moving it to a new archetype.
    ///
    /// If the entity's last component is removed, the entity is destroyed
    /// instead of migrated to an empty archetype.
    ///
    /// # Errors
    ///
    /// Returns [`RemoveComponentError::EntityNotFound`] if the entity does
    /// not exist, and [`RemoveComponentError::ComponentNotFound`] if the
    /// entity does not carry the component `T`.
    pub fn remove_component<T: Component>(
        &mut self,
        entity: Entity,
    ) -> Result<(), RemoveComponentError> {
        self.remove_component_by_id(entity, ComponentId::of::<T>())
    }

    /// Remove a component identified by [`ComponentId`] from one entity.
    ///
    /// This is the non-generic core of [`Self::remove_component`]; the host
    /// uses it when dropping data for a type it can no longer name statically
    /// (a component a reloaded module stopped registering).
    pub(crate) fn remove_component_by_id(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
    ) -> Result<(), RemoveComponentError> {
        let _zone =
            crate::profile_scope!("remove component", [("Entity being mutated: {:?}", entity)]);

        // Step 1: Validate - the entity must exist and currently carry the
        // component.
        let location = match self.entity_locations.get(&entity) {
            Some(loc) => *loc,
            None => return Err(RemoveComponentError::EntityNotFound),
        };

        let old_archetype = match self.archetypes.get(&location.archetype_id) {
            Some(arch) => arch,
            None => return Err(RemoveComponentError::EntityNotFound),
        };

        // Check if entity has this component
        if !old_archetype.component_types.contains(&component_id) {
            return Err(RemoveComponentError::ComponentNotFound);
        }

        // Step 2: Build the destination component set without T. If no
        // components remain, destroy the entity instead of migrating it to
        // an empty archetype.
        let new_component_ids: Vec<ComponentId> = old_archetype
            .component_types
            .iter()
            .filter(|&id| *id != component_id)
            .cloned()
            .collect();

        // If no components left, destroy the entity instead.
        // The entity may already be gone; that's fine - we just need to
        // stop trying to migrate it.
        if new_component_ids.is_empty() {
            let _ = self.destroy_entity(entity);
            return Ok(());
        }

        // Step 3: Migrate the entity to the new archetype, copying every
        // remaining component through its registered copier.
        let copiers: Vec<_> = new_component_ids
            .iter()
            .filter_map(|component_id| self.component_copiers.get(component_id).copied())
            .collect();

        // Move entity to new archetype without the removed component
        self.move_entity_to_archetype(
            entity,
            new_component_ids,
            |old_storage, new_storage, old_index| {
                // Copy all components except the removed one
                for copier in copiers.iter() {
                    copier(old_storage, new_storage, old_index);
                }
            },
        )
        .unwrap_or_else(|error| {
            // The entity and its archetype were validated above, so a
            // migration failure here is an internal-invariant break, not a
            // user error. Name the entity and the failure so the report is
            // startable; the descriptor paths propagate the same failure as a
            // typed error instead, because their inputs come from outside
            // the engine.
            panic!("internal invariant broken while migrating entity {entity:?}: {error}")
        });

        Ok(())
    }

    /// Add a component to an existing entity, moving it to a new archetype.
    ///
    /// Existing components are preserved during the migration.
    ///
    /// # Errors
    ///
    /// Returns [`AddComponentError::EntityNotFound`] if the entity does not
    /// exist, and [`AddComponentError::ComponentAlreadyExists`] if the entity
    /// already carries the component `T`.
    pub fn add_component<T>(
        &mut self,
        entity: Entity,
        component: T,
    ) -> Result<(), AddComponentError>
    where
        T: Component + Clone,
    {
        let _zone = crate::profile_scope!(
            "add component",
            [(
                "Target entity: {:?}, Component type being added: {}",
                entity,
                std::any::type_name::<T>()
            )]
        );
        let component_id = ComponentId::of::<T>();

        // Step 1: Validate - the entity must exist and must not already
        // carry T.
        let location = match self.entity_locations.get(&entity) {
            Some(loc) => *loc,
            None => return Err(AddComponentError::EntityNotFound),
        };

        let old_archetype = match self.archetypes.get(&location.archetype_id) {
            Some(arch) => arch,
            None => return Err(AddComponentError::EntityNotFound),
        };

        // Check if entity already has this component
        if old_archetype.component_types.contains(&component_id) {
            return Err(AddComponentError::ComponentAlreadyExists);
        }

        // Step 2: Build the destination component set with T appended.
        let mut new_component_ids = Vec::with_capacity(old_archetype.component_types.len() + 1);
        new_component_ids.extend_from_slice(&old_archetype.component_types);
        new_component_ids.push(component_id);
        new_component_ids.sort();

        // Step 3: Migrate the entity, copying existing components through
        // their copiers and pushing the new component value.
        let copiers: Vec<_> = old_archetype
            .component_types
            .iter()
            .filter_map(|component_id| self.component_copiers.get(component_id).copied())
            .collect();

        // Move entity to new archetype with the additional component
        self.move_entity_to_archetype(
            entity,
            new_component_ids,
            |old_storage, new_storage, old_index| {
                // Copy all existing components
                for copier in copiers.iter() {
                    copier(old_storage, new_storage, old_index);
                }
                // Add the new component
                new_storage.column_of_mut::<T>().push::<T>(component);
            },
        )
        .unwrap_or_else(|error| {
            // As in `remove_component_by_id`: the entity and its archetype
            // were validated above, so this failure is an internal-invariant
            // break, not a user error.
            panic!("internal invariant broken while migrating entity {entity:?}: {error}")
        });

        Ok(())
    }

    /// Add a runtime-defined component and migrate the entity's other columns.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::EntityNotFound`] if the entity does not exist,
    /// [`WorldError::DescriptorComponentAlreadyPresent`] if the entity already
    /// carries the component, [`WorldError::DescriptorComponentNotRegistered`]
    /// if the component was never registered, and
    /// [`WorldError::DescriptorByteLengthMismatch`] if `bytes` does not match
    /// the registered layout size.
    pub fn add_descriptor_component(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
        bytes: &[u8],
    ) -> Result<(), WorldError> {
        let location = *self
            .entity_locations
            .get(&entity)
            .ok_or(WorldError::EntityNotFound)?;
        let Some(old_archetype) = self.archetypes.get(&location.archetype_id) else {
            return Err(WorldError::ArchetypeMissing {
                entity,
                archetype_id: location.archetype_id,
            });
        };
        if old_archetype.component_types.contains(&component_id) {
            return Err(WorldError::DescriptorComponentAlreadyPresent);
        }
        let expected_size = match self.storage_factories.get(&component_id) {
            Some(StorageFactory::Descriptor(layout)) => layout.size,
            _ => return Err(WorldError::DescriptorComponentNotRegistered { id: component_id }),
        };
        if bytes.len() != expected_size {
            return Err(WorldError::DescriptorByteLengthMismatch { id: component_id });
        }
        let mut new_ids = old_archetype.component_types.clone();
        new_ids.push(component_id);
        new_ids.sort();
        let copiers: Vec<_> = old_archetype
            .component_types
            .iter()
            .filter_map(|id| self.component_copiers.get(id).copied())
            .collect();
        self.move_entity_to_archetype(entity, new_ids, |old, new, index| {
            for copier in &copiers {
                copier(old, new, index);
            }
        })?;
        // Re-resolve the location: the migration above moved the entity into
        // the destination archetype, so the pre-move location is stale.
        let Some(&location) = self.entity_locations.get(&entity) else {
            return Err(WorldError::EntityNotFound);
        };
        let Some(archetype) = self.archetypes.get_mut(&location.archetype_id) else {
            return Err(WorldError::ArchetypeMissing {
                entity,
                archetype_id: location.archetype_id,
            });
        };
        let Some(storage) = archetype.component_storages.get_mut(component_id) else {
            return Err(WorldError::DescriptorStorageMissing {
                component_id,
                archetype_id: location.archetype_id,
            });
        };
        storage.set_bytes(location.index_in_archetype, bytes)?;
        Ok(())
    }

    /// Replace the bytes of an existing runtime-defined component row.
    pub(crate) fn set_descriptor_component_bytes(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
        bytes: &[u8],
    ) -> Result<(), WorldError> {
        let location = *self
            .entity_locations
            .get(&entity)
            .ok_or(WorldError::EntityNotFound)?;
        self.archetypes
            .get_mut(&location.archetype_id)
            .and_then(|archetype| archetype.component_storages.get_mut(component_id))
            .ok_or(WorldError::DescriptorComponentMissing)?
            .set_bytes(location.index_in_archetype, bytes)
    }

    /// Add a zero-initialized runtime-defined component.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DescriptorComponentNotRegistered`] if the component
    /// was never registered, plus any error reported by
    /// [`add_descriptor_component`](Self::add_descriptor_component).
    pub fn add_descriptor_component_default(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
    ) -> Result<(), WorldError> {
        let size = match self.storage_factories.get(&component_id) {
            Some(StorageFactory::Descriptor(layout)) => layout.size,
            _ => return Err(WorldError::DescriptorComponentNotRegistered { id: component_id }),
        };
        self.add_descriptor_component(entity, component_id, &vec![0; size])
    }

    /// Remove a runtime-defined component while preserving every other column.
    ///
    /// If the entity's last component is removed, the entity is destroyed
    /// instead of migrated to an empty archetype.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::EntityNotFound`] if the entity does not exist,
    /// and [`WorldError::DescriptorComponentMissing`] if the entity does not
    /// carry the component.
    pub fn remove_descriptor_component(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
    ) -> Result<(), WorldError> {
        let location = *self
            .entity_locations
            .get(&entity)
            .ok_or(WorldError::EntityNotFound)?;
        let Some(old_archetype) = self.archetypes.get(&location.archetype_id) else {
            return Err(WorldError::ArchetypeMissing {
                entity,
                archetype_id: location.archetype_id,
            });
        };
        if !old_archetype.component_storages.contains(component_id) {
            return Err(WorldError::DescriptorComponentMissing);
        }
        let new_ids: Vec<_> = old_archetype
            .component_types
            .iter()
            .copied()
            .filter(|id| *id != component_id)
            .collect();
        if new_ids.is_empty() {
            let _ = self.destroy_entity(entity);
            return Ok(());
        }
        let copiers: Vec<_> = new_ids
            .iter()
            .filter_map(|id| self.component_copiers.get(id).copied())
            .collect();
        self.move_entity_to_archetype(entity, new_ids, |old, new, index| {
            for copier in &copiers {
                copier(old, new, index);
            }
        })?;
        Ok(())
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

    /// Print information about all archetypes in the world
    ///
    /// This displays the component types and entity count for each archetype,
    /// useful for debugging and understanding the current state of the ECS.
    pub fn print_archetypes(&self) {
        println!(
            "\n=== World Archetypes (Total: {}) ===",
            self.archetypes.len()
        );
        for (_, archetype) in self.archetypes.iter() {
            archetype.print_info(&self.component_registry);
        }
        println!("Total entities: {}", self.entity_locations.len());
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

    /// Read-only view of the component registry.
    ///
    /// Exposed for the same reason as [`World::archetypes_iter`]: resolving a
    /// component column by stable type name and size rather than by a `TypeId`,
    /// which differs between the host and a hot-loaded DLL.
    #[inline]
    pub fn component_registry(&self) -> &ComponentRegistry {
        &self.component_registry
    }

    /// Estimate the total memory footprint of the world in bytes.
    ///
    /// Sums all archetype storage, entity location map, resources,
    /// and internal data structures.
    pub fn memory_estimate(&self) -> usize {
        let mut total = 0usize;

        // Archetype storage (component columns + entities + ticks)
        for archetype in self.archetypes.values() {
            total += archetype.memory_estimate(&self.component_registry);
        }

        // Entity location map: ~32 bytes per entry (Entity key + EntityLocation value + HashMap overhead)
        total += self.entity_locations.len() * 48;

        // Resources: approximate based on type count
        total += self.resources.len() * 128;

        // Free entity IDs
        total += self.free_entity_ids.capacity() * 12;

        // Storage factories, copiers, script data
        total += self.storage_factories.len() * 128;
        total += self.component_copiers.len() * 16;
        total += self.script_components.len() * 24;
        total += self.script_updaters.len() * 32;

        total
    }

    /// Generate a Graphviz DOT representation of the world for debugging.
    ///
    /// Useful for debugging archetype fragmentation and visualizing the
    /// relationship between component sets and entity counts. The output is
    /// a `digraph` with one node per archetype, labelled with its component
    /// names and entity count.
    ///
    /// # Example output
    ///
    /// ```dot
    /// digraph World {
    ///     rankdir=LR;
    ///     node [shape=record];
    ///     "arch_0" [label="Position, Velocity | 3 entities"];
    ///     "arch_1" [label="Position, Health | 1 entity"];
    /// }
    /// ```
    #[cold]
    pub fn to_dot_graph(&self) -> String {
        let mut dot = String::from("digraph World {\n    rankdir=LR;\n    node [shape=record];\n");

        for (_, archetype) in self.archetypes.iter() {
            let component_names: Vec<String> = archetype
                .component_types
                .iter()
                .map(|id| {
                    self.component_registry
                        .get_name(id)
                        .unwrap_or("?")
                        .to_string()
                })
                .collect();
            let label = format!(
                "{} | {} entit{}",
                component_names.join(", "),
                archetype.entities.len(),
                if archetype.entities.len() == 1 {
                    "y"
                } else {
                    "ies"
                }
            );
            dot.push_str(&format!(
                "    \"arch_{:?}\" [label=\"{}\"];\n",
                archetype.id.0, label
            ));
        }

        dot.push_str("}\n");
        dot
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
// Free Functions
// =============================================================================

/// Copies a single component instance from source to destination storage.
fn copy_component<T: Component + Clone>(
    source: &ComponentColumns,
    destination: &mut ComponentColumns,
    index: usize,
) {
    let component = source.column_of::<T>().get::<T>(index);
    destination
        .column_of_mut::<T>()
        .push::<T>(component.clone());
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
mod tests {
    use super::*;
    use crate::archetype::{FieldSource, LayoutField};

    /// The change-detection ticks of one entity's row for one component.
    fn ticks_of_row(world: &World, entity: Entity, component: ComponentId) -> ComponentTicks {
        let location = world.entity_locations[&entity];
        world.archetypes[&location.archetype_id].component_ticks[&component]
            [location.index_in_archetype]
    }

    /// Two u32 fields: the two-field shape the relayout tests start from.
    fn two_u32_row(first: u32, second: u32) -> Vec<u8> {
        let mut bytes = first.to_ne_bytes().to_vec();
        bytes.extend_from_slice(&second.to_ne_bytes());
        bytes
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    struct Position {
        x: f32,
        y: f32,
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    struct Velocity {
        x: f32,
        y: f32,
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    struct Health {
        hp: i32,
    }

    impl Component for Position {}
    impl Component for Velocity {}
    impl Component for Health {}

    /// A resource used to pin registration-stamp behaviour.
    #[derive(Debug, Default)]
    struct AccountedResource {
        value: u32,
    }
    impl crate::resource::Resource for AccountedResource {}

    /// A second one, so a fresh registration can be told from a replacement.
    #[derive(Debug, Default)]
    struct FreshAccountedResource {
        value: u32,
    }
    impl crate::resource::Resource for FreshAccountedResource {}

    #[test]
    fn descriptor_components_coexist_and_survive_archetype_migration() {
        let mut world = World::new();
        let a = world
            .register_component_descriptor(
                0xA1,
                "Project.A",
                4,
                4,
                11,
                Blittability::engine_verified(),
            )
            .unwrap();
        let b = world
            .register_component_descriptor(
                0xB2,
                "Project.B",
                4,
                4,
                22,
                Blittability::engine_verified(),
            )
            .unwrap();
        let c = world
            .register_component_descriptor(
                0xC3,
                "Project.C",
                8,
                8,
                33,
                Blittability::engine_verified(),
            )
            .unwrap();
        let entity = world
            .create_descriptor_entity(&[
                (a, 10_u32.to_ne_bytes().to_vec()),
                (b, 20_u32.to_ne_bytes().to_vec()),
            ])
            .unwrap();

        assert_eq!(
            world.descriptor_component_bytes(entity, a).unwrap(),
            10_u32.to_ne_bytes()
        );
        assert_eq!(
            world.descriptor_component_bytes(entity, b).unwrap(),
            20_u32.to_ne_bytes()
        );

        world.add_descriptor_component_default(entity, c).unwrap();
        assert_eq!(
            world.descriptor_component_bytes(entity, a).unwrap(),
            10_u32.to_ne_bytes()
        );
        assert_eq!(
            world.descriptor_component_bytes(entity, b).unwrap(),
            20_u32.to_ne_bytes()
        );
        assert_eq!(world.descriptor_component_bytes(entity, c).unwrap(), [0; 8]);

        world.remove_descriptor_component(entity, b).unwrap();
        assert_eq!(
            world.descriptor_component_bytes(entity, a).unwrap(),
            10_u32.to_ne_bytes()
        );
        assert!(world.descriptor_component_bytes(entity, b).is_none());
        assert_eq!(world.descriptor_component_bytes(entity, c).unwrap(), [0; 8]);
    }

    /// A relayout rewrites rows where they are: entities keep their rows, values
    /// follow their field names, and a field the old layout did not have starts
    /// zeroed instead of holding a byte from the previous shape.
    #[test]
    fn relayout_migrates_rows_and_keeps_entities_in_their_rows() {
        let mut world = World::new();
        let component = world
            .register_component_descriptor(
                0xD4,
                "Project.Relayout",
                8,
                4,
                100,
                Blittability::engine_verified(),
            )
            .unwrap();
        let first = world
            .create_descriptor_entity(&[(component, two_u32_row(1, 2))])
            .unwrap();
        let second = world
            .create_descriptor_entity(&[(component, two_u32_row(3, 4))])
            .unwrap();

        let archetype = world.entity_locations[&first].archetype_id;
        let first_row = world.entity_locations[&first].index_in_archetype;
        let second_row = world.entity_locations[&second].index_in_archetype;
        assert_eq!(
            world.entity_locations[&second].archetype_id, archetype,
            "identical component sets share one archetype"
        );

        // `b` first, then `a`, then an eight-byte field that did not exist.
        let plan = FieldPlan::between(
            &[
                LayoutField {
                    name: "a",
                    type_tag: "u32",
                    offset: 0,
                    size: 4,
                },
                LayoutField {
                    name: "b",
                    type_tag: "u32",
                    offset: 4,
                    size: 4,
                },
            ],
            &[
                LayoutField {
                    name: "b",
                    type_tag: "u32",
                    offset: 0,
                    size: 4,
                },
                LayoutField {
                    name: "a",
                    type_tag: "u32",
                    offset: 4,
                    size: 4,
                },
                LayoutField {
                    name: "added",
                    type_tag: "u32",
                    offset: 8,
                    size: 8,
                },
            ],
        );

        let migrated = world
            .relayout_descriptor_component(component, 16, 8, 200, &plan)
            .unwrap();

        assert_eq!(migrated, 2);
        let mut expected_first = two_u32_row(2, 1);
        expected_first.extend_from_slice(&[0_u8; 8]);
        assert_eq!(
            world.descriptor_component_bytes(first, component).unwrap(),
            expected_first.as_slice()
        );
        let mut expected_second = two_u32_row(4, 3);
        expected_second.extend_from_slice(&[0_u8; 8]);
        assert_eq!(
            world.descriptor_component_bytes(second, component).unwrap(),
            expected_second.as_slice()
        );
        assert_eq!(
            world.entity_locations[&first].archetype_id, archetype,
            "a relayout moves no entity between archetypes"
        );
        assert_eq!(world.entity_locations[&first].index_in_archetype, first_row);
        assert_eq!(
            world.entity_locations[&second].index_in_archetype,
            second_row
        );
        assert_eq!(world.component_layout(component), Some((16, 8)));
    }

    /// A relayout is a structural edit, not an add: rows keep their ticks, so no
    /// `Added` filter fires and a row that was already changed stays changed.
    #[test]
    fn relayout_leaves_change_ticks_alone() {
        let mut world = World::new();
        let component = world
            .register_component_descriptor(
                0xD5,
                "Project.Ticks",
                8,
                4,
                100,
                Blittability::engine_verified(),
            )
            .unwrap();
        let entity = world
            .create_descriptor_entity(&[(component, two_u32_row(1, 2))])
            .unwrap();

        let changed_tick = world.increment_change_tick();
        let location = world.entity_locations[&entity];
        world
            .archetypes
            .get_mut(&location.archetype_id)
            .unwrap()
            .component_ticks
            .get_mut(&component)
            .unwrap()[location.index_in_archetype]
            .set_changed(changed_tick);
        let before = ticks_of_row(&world, entity, component);

        world
            .relayout_descriptor_component(component, 16, 8, 200, &FieldPlan::new())
            .unwrap();

        let after = ticks_of_row(&world, entity, component);
        assert_eq!(after.added, before.added);
        assert_eq!(after.changed, changed_tick);
    }

    /// A remap carries rows from the predecessor registration to the
    /// successor: the values move, the successor answers to its own name, the
    /// predecessor's registration is retired, and an entity whose only
    /// component this is survives the move.
    #[test]
    fn remap_moves_rows_to_the_successor_and_retires_the_source() {
        let mut world = World::new();
        let old = world
            .register_component_descriptor(
                0xE5,
                "Project.OldName",
                8,
                4,
                200,
                Blittability::engine_verified(),
            )
            .unwrap();
        let new = world
            .register_component_descriptor(
                0xF6,
                "Project.NewName",
                8,
                4,
                200,
                Blittability::engine_verified(),
            )
            .unwrap();
        let companion = world
            .register_component_descriptor(
                0x1A,
                "Project.Companion",
                4,
                4,
                9,
                Blittability::engine_verified(),
            )
            .unwrap();
        // One entity carries only the component being moved: the
        // add-before-remove order is what keeps it alive.
        let alone = world
            .create_descriptor_entity(&[(old, two_u32_row(7, 8))])
            .unwrap();
        let paired = world
            .create_descriptor_entity(&[
                (old, two_u32_row(1, 2)),
                (companion, 3_u32.to_ne_bytes().to_vec()),
            ])
            .unwrap();

        let fields = vec![
            LayoutField {
                name: "a",
                type_tag: "u32",
                offset: 0,
                size: 4,
            },
            LayoutField {
                name: "b",
                type_tag: "u32",
                offset: 4,
                size: 4,
            },
        ];
        let plan = FieldPlan::between(&fields, &fields);
        let moved = world.remap_descriptor_component(old, new, &plan).unwrap();

        assert_eq!(moved, 2);
        assert_eq!(
            world.descriptor_component_bytes(alone, new).unwrap(),
            two_u32_row(7, 8)
        );
        assert_eq!(
            world.descriptor_component_bytes(paired, new).unwrap(),
            two_u32_row(1, 2)
        );
        assert_eq!(
            world.descriptor_component_bytes(paired, companion).unwrap(),
            3_u32.to_ne_bytes(),
            "the companion column survives the move"
        );
        assert!(world.descriptor_component_bytes(alone, old).is_none());
        assert!(
            !world.storage_factories.contains_key(&old),
            "the predecessor's registration is retired"
        );
        assert_eq!(
            world
                .resolve_component_id_by_name_any("Project.OldName")
                .unwrap(),
            None,
            "the old name is free again"
        );
        assert_eq!(
            world
                .resolve_component_id_by_name_any("Project.NewName")
                .unwrap(),
            Some(new)
        );
    }

    /// The plan's field mapping is what moves, not the bytes: a shape with a
    /// moved field and an added one reorders and zero-fills, exactly as a
    /// relayout would.
    #[test]
    fn remap_reshapes_rows_through_the_plan() {
        let mut world = World::new();
        let old = world
            .register_component_descriptor(
                0xE6,
                "Project.PlanOld",
                8,
                4,
                300,
                Blittability::engine_verified(),
            )
            .unwrap();
        let new = world
            .register_component_descriptor(
                0xF7,
                "Project.PlanNew",
                12,
                4,
                301,
                Blittability::engine_verified(),
            )
            .unwrap();
        let entity = world
            .create_descriptor_entity(&[(old, two_u32_row(1, 2))])
            .unwrap();

        let plan = FieldPlan::between(
            &[
                LayoutField {
                    name: "a",
                    type_tag: "u32",
                    offset: 0,
                    size: 4,
                },
                LayoutField {
                    name: "b",
                    type_tag: "u32",
                    offset: 4,
                    size: 4,
                },
            ],
            &[
                LayoutField {
                    name: "b",
                    type_tag: "u32",
                    offset: 0,
                    size: 4,
                },
                LayoutField {
                    name: "a",
                    type_tag: "u32",
                    offset: 4,
                    size: 4,
                },
                LayoutField {
                    name: "added",
                    type_tag: "u32",
                    offset: 8,
                    size: 4,
                },
            ],
        );
        world.remap_descriptor_component(old, new, &plan).unwrap();

        let mut expected = 2_u32.to_ne_bytes().to_vec();
        expected.extend_from_slice(&1_u32.to_ne_bytes());
        expected.extend_from_slice(&0_u32.to_ne_bytes());
        assert_eq!(
            world.descriptor_component_bytes(entity, new).unwrap(),
            expected.as_slice()
        );
    }

    /// A plan default rides the plan: an added field is filled with the
    /// supplied bytes instead of zero, and setting one on a copied field is a
    /// no-op rather than an overwrite.
    #[test]
    fn a_plan_default_fills_an_added_field_and_never_overrides_a_copied_one() {
        let mut world = World::new();
        let component = world
            .register_component_descriptor(
                0xD8,
                "Project.Defaults",
                8,
                4,
                100,
                Blittability::engine_verified(),
            )
            .unwrap();
        let entity = world
            .create_descriptor_entity(&[(component, two_u32_row(1, 2))])
            .unwrap();

        let mut plan = FieldPlan::between(
            &[
                LayoutField {
                    name: "a",
                    type_tag: "u32",
                    offset: 0,
                    size: 4,
                },
                LayoutField {
                    name: "b",
                    type_tag: "u32",
                    offset: 4,
                    size: 4,
                },
            ],
            &[
                LayoutField {
                    name: "a",
                    type_tag: "u32",
                    offset: 0,
                    size: 4,
                },
                LayoutField {
                    name: "b",
                    type_tag: "u32",
                    offset: 4,
                    size: 4,
                },
                LayoutField {
                    name: "added",
                    type_tag: "u32",
                    offset: 8,
                    size: 4,
                },
            ],
        );
        plan.set_default(8, &6_u32.to_ne_bytes())
            .expect("the added field has a planned slot");
        // A default on a copied field is accepted and ignored, so the carried
        // value survives even a default declared for it.
        plan.set_default(0, &9_u32.to_ne_bytes())
            .expect("setting on a copied field is accepted");

        world
            .relayout_descriptor_component(component, 12, 4, 101, &plan)
            .unwrap();

        let mut expected = 1_u32.to_ne_bytes().to_vec();
        expected.extend_from_slice(&2_u32.to_ne_bytes());
        expected.extend_from_slice(&6_u32.to_ne_bytes());
        assert_eq!(
            world.descriptor_component_bytes(entity, component).unwrap(),
            expected.as_slice(),
            "the added field takes its default and the copied fields keep their values"
        );
    }

    /// Retiring a descriptor component's storage removes its rows and its
    /// registration, and frees the name and the bit for a later declaration -
    /// the contract a managed manifest's removed type rides on.
    #[test]
    fn retire_component_storage_frees_a_descriptor_component() {
        let mut world = World::new();
        let component = world
            .register_component_descriptor(
                0xB9,
                "Project.Retired",
                4,
                4,
                400,
                Blittability::engine_verified(),
            )
            .unwrap();
        let companion = world
            .register_component_descriptor(
                0xCA,
                "Project.Stays",
                4,
                4,
                401,
                Blittability::engine_verified(),
            )
            .unwrap();
        let entity = world
            .create_descriptor_entity(&[
                (component, 5_u32.to_ne_bytes().to_vec()),
                (companion, 6_u32.to_ne_bytes().to_vec()),
            ])
            .unwrap();

        let affected = world.retire_component_storage(&[component]);

        assert_eq!(affected, 1, "the retired component's row went");
        assert!(world
            .descriptor_component_bytes(entity, component)
            .is_none());
        assert_eq!(
            world.descriptor_component_bytes(entity, companion).unwrap(),
            6_u32.to_ne_bytes(),
            "the companion column survives the retirement"
        );
        assert!(
            !world.storage_factories.contains_key(&component),
            "the registration is forgotten"
        );
        assert_eq!(
            world
                .resolve_component_id_by_name_any("Project.Retired")
                .unwrap(),
            None,
            "the name is free again"
        );

        // The name and the bit can be taken by a later declaration.
        let replacement = world
            .register_component_descriptor(
                0xCB,
                "Project.Retired",
                4,
                4,
                402,
                Blittability::engine_verified(),
            )
            .unwrap();
        assert_eq!(
            world
                .resolve_component_id_by_name_any("Project.Retired")
                .unwrap(),
            Some(replacement)
        );
    }

    /// The resource twin: the value moves onto the successor's id, claims
    /// travel with it, and the source declaration is dropped once they have.
    #[test]
    fn remap_foreign_resource_moves_the_value_and_the_claim() {
        let mut world = World::new();
        let old = world
            .register_foreign_resource("Project.OldSettings", "Project.OldSettings", 8, 4, 77)
            .unwrap();
        world
            .insert_foreign_resource_bytes(old, &two_u32_row(5, 6))
            .unwrap();
        let new = world
            .register_foreign_resource("Project.NewSettings", "Project.NewSettings", 8, 4, 77)
            .unwrap();
        // The host seeds a fresh declaration with zeroes; the moved value
        // replaces that seed.
        world
            .insert_foreign_resource_bytes(new, &[0_u8; 8])
            .unwrap();
        world.retain_resource_claims(&[old]);

        let fields = vec![
            LayoutField {
                name: "a",
                type_tag: "u32",
                offset: 0,
                size: 4,
            },
            LayoutField {
                name: "b",
                type_tag: "u32",
                offset: 4,
                size: 4,
            },
        ];
        let plan = FieldPlan::between(&fields, &fields);
        let moved = world.remap_foreign_resource(old, new, &plan).unwrap();

        assert_eq!(moved, 1);
        assert_eq!(
            world.foreign_resource_bytes(new).unwrap(),
            two_u32_row(5, 6)
        );
        assert!(
            world.foreign_resource_layout(old).is_none(),
            "the source declaration is dropped once its claims moved"
        );
        assert_eq!(world.resource_claim_counts.get(&new).copied(), Some(1));
        assert!(!world.resource_claim_counts.contains_key(&old));
    }

    /// The id and the registry bit survive, because they are baked into
    /// archetype masks and scheduled access masks: a relayout must update the
    /// layout in place, never re-register the component.
    #[test]
    fn relayout_keeps_the_component_id_and_its_bit() {
        let mut world = World::new();
        let component = world
            .register_component_descriptor(
                0xD6,
                "Project.Bit",
                8,
                4,
                100,
                Blittability::engine_verified(),
            )
            .unwrap();
        let bit = world.component_registry().get_bit(&component).unwrap();

        world
            .relayout_descriptor_component(component, 16, 8, 200, &FieldPlan::new())
            .unwrap();

        // The same id still names the same component; only its size moved.
        assert_eq!(world.component_registry().get_bit(&component), Some(bit));
        assert_eq!(world.component_registry().get_size(&component), Some(16));
        assert_eq!(
            world.component_registry().get_name(&component),
            Some("Project.Bit")
        );
    }

    /// A relayout has to reach archetypes whose last row is gone: the next
    /// entity added there must be stored at the new size. The push only
    /// succeeds when the column there really was rewritten.
    #[test]
    fn relayout_reaches_an_archetype_that_lost_its_last_row() {
        let mut world = World::new();
        let component = world
            .register_component_descriptor(
                0xD7,
                "Project.Empty",
                8,
                4,
                100,
                Blittability::engine_verified(),
            )
            .unwrap();
        let disposable = world
            .create_descriptor_entity(&[(component, two_u32_row(1, 2))])
            .unwrap();
        let archetype = world.entity_locations[&disposable].archetype_id;
        assert!(world.destroy_entity(disposable));

        world
            .relayout_descriptor_component(component, 16, 8, 200, &FieldPlan::new())
            .unwrap();

        let entity = world
            .create_descriptor_entity(&[(component, vec![7_u8; 16])])
            .unwrap();
        assert_eq!(
            world.entity_locations[&entity].archetype_id, archetype,
            "the empty archetype is reused, so its column is the one being tested"
        );
        assert_eq!(
            world.descriptor_component_bytes(entity, component).unwrap(),
            [7_u8; 16].as_slice()
        );
    }

    /// Only a registered descriptor component has a layout to replace, and a plan
    /// that does not fit both layouts is refused before anything is touched.
    #[test]
    fn relayout_refuses_what_it_cannot_migrate() {
        let mut world = World::new();
        world.register_component::<Position>();
        assert!(matches!(
            world.relayout_descriptor_component(
                ComponentId::of::<Position>(),
                16,
                8,
                1,
                &FieldPlan::new()
            ),
            Err(WorldError::DescriptorComponentNotRegistered { .. })
        ));
        assert!(matches!(
            world.relayout_descriptor_component(
                ComponentId::descriptor(0xEE),
                16,
                8,
                1,
                &FieldPlan::new()
            ),
            Err(WorldError::DescriptorComponentNotRegistered { .. })
        ));

        let component = world
            .register_component_descriptor(
                0xD8,
                "Project.Refused",
                8,
                4,
                100,
                Blittability::engine_verified(),
            )
            .unwrap();
        let mut too_wide = FieldPlan::new();
        too_wide.push(4, 8, FieldSource::OldOffset(0));
        assert!(matches!(
            world.relayout_descriptor_component(component, 8, 4, 100, &too_wide),
            Err(WorldError::DescriptorRowInvalid)
        ));
        assert_eq!(
            world.component_layout(component),
            Some((8, 4)),
            "a refused plan leaves the registered layout as it was"
        );

        // A column that goes missing is reported before the first migration,
        // so the archetypes holding rows keep them. The destination archetype
        // is made to list the component without storing a column for it.
        let holder = world
            .create_descriptor_entity(&[(component, 8_u64.to_ne_bytes().to_vec())])
            .unwrap();
        let stripped =
            world.get_or_create_archetype(vec![component, ComponentId::of::<Position>()]);
        assert!(world
            .archetypes
            .get_mut(&stripped)
            .unwrap()
            .component_storages
            .remove(component)
            .is_some());
        let row_before = world
            .descriptor_component_bytes(holder, component)
            .expect("the entity has a row")
            .to_vec();
        assert!(matches!(
            world.relayout_descriptor_component(component, 8, 4, 100, &FieldPlan::new()),
            Err(WorldError::DescriptorStorageMissing { .. })
        ));
        assert_eq!(
            world.descriptor_component_bytes(holder, component),
            Some(row_before.as_slice()),
            "a refused relayout left the row it would have migrated alone"
        );

        // Put the stripped column back, then drift one behind the factory's
        // back: the size mismatch is reported too, and the column keeps its
        // drifted shape instead of being silently rewritten.
        world
            .archetypes
            .get_mut(&stripped)
            .unwrap()
            .component_storages
            .insert(
                component,
                crate::archetype::ComponentColumn::new(ComponentLayout {
                    size: 8,
                    align: 4,
                    schema_hash: 100,
                    blittability: Blittability::engine_verified(),
                })
                .expect("the drifted layout is still a valid allocation layout"),
            );
        {
            let location = world.entity_locations[&holder];
            let column = world
                .archetypes
                .get_mut(&location.archetype_id)
                .unwrap()
                .component_storages
                .get_mut(component)
                .unwrap();
            column.relayout_validated(
                ComponentLayout {
                    size: 16,
                    align: 4,
                    schema_hash: 100,
                    blittability: Blittability::engine_verified(),
                },
                &FieldPlan::new(),
                8,
            );
        }
        assert!(matches!(
            world.relayout_descriptor_component(component, 8, 4, 100, &FieldPlan::new()),
            Err(WorldError::ComponentColumnLayoutMismatch { .. })
        ));
        assert_eq!(
            world.archetypes[&world.entity_locations[&holder].archetype_id]
                .component_storages
                .get(component)
                .expect("column")
                .element_size(),
            16,
            "the mismatched column was reported, not rewritten"
        );
        assert_eq!(
            world.component_layout(component),
            Some((8, 4)),
            "the factory still carries the registered layout"
        );
    }

    /// A failed migration is atomic: the pre-flight refuses the destination
    /// before the entity's row, its columns or its location move anywhere.
    #[test]
    fn migration_failure_leaves_both_archetypes_untouched() {
        let mut world = World::new();
        world.register_component::<Position>();
        let first = world
            .register_component_descriptor(
                0xE1,
                "Project.First",
                4,
                4,
                1,
                Blittability::engine_verified(),
            )
            .unwrap();
        let second = world
            .register_component_descriptor(
                0xE2,
                "Project.Second",
                4,
                4,
                2,
                Blittability::engine_verified(),
            )
            .unwrap();

        // Make the {first, second} destination exist, then manufacture the
        // desync: it lists `second` without storing a column for it.
        let destination = world.get_or_create_archetype(vec![first, second]);
        assert!(world
            .archetypes
            .get_mut(&destination)
            .unwrap()
            .component_storages
            .remove(second)
            .is_some());

        let entity = world
            .create_descriptor_entity(&[(first, 7_u32.to_ne_bytes().to_vec())])
            .unwrap();
        let source = world.entity_locations[&entity].archetype_id;
        let source_rows = world.archetypes[&source].entities.len();
        let source_column = world.archetypes[&source]
            .component_storages
            .get(first)
            .expect("column")
            .len();
        let destination_rows = world.archetypes[&destination].entities.len();

        assert!(matches!(
            world.add_descriptor_component_default(entity, second),
            Err(WorldError::DescriptorStorageMissing { .. })
        ));

        assert_eq!(world.entity_locations[&entity].archetype_id, source);
        assert_eq!(
            world.archetypes[&source].entities.len(),
            source_rows,
            "the source kept its row while the destination refused the migration"
        );
        assert_eq!(
            world.archetypes[&source]
                .component_storages
                .get(first)
                .expect("column")
                .len(),
            source_column
        );
        assert_eq!(
            world.archetypes[&destination].entities.len(),
            destination_rows,
            "the destination never received the entity"
        );
    }

    #[test]
    fn descriptor_component_ticks_survive_archetype_migration() {
        fn ticks_for(world: &World, entity: Entity, component: ComponentId) -> ComponentTicks {
            let location = world.entity_locations[&entity];
            world.archetypes[&location.archetype_id].component_ticks[&component]
                [location.index_in_archetype]
        }

        let mut world = World::new();
        let retained = world
            .register_component_descriptor(
                0xA1,
                "Project.Retained",
                4,
                4,
                11,
                Blittability::engine_verified(),
            )
            .unwrap();
        let removed = world
            .register_component_descriptor(
                0xB2,
                "Project.Removed",
                4,
                4,
                22,
                Blittability::engine_verified(),
            )
            .unwrap();
        let added = world
            .register_component_descriptor(
                0xC3,
                "Project.Added",
                8,
                8,
                33,
                Blittability::engine_verified(),
            )
            .unwrap();
        let entity = world
            .create_descriptor_entity(&[
                (retained, 10_u32.to_ne_bytes().to_vec()),
                (removed, 20_u32.to_ne_bytes().to_vec()),
            ])
            .unwrap();

        let original_retained_ticks = ticks_for(&world, entity, retained);
        let original_removed_ticks = ticks_for(&world, entity, removed);
        let changed_tick = world.increment_change_tick();
        let location = world.entity_locations[&entity];
        world
            .archetypes
            .get_mut(&location.archetype_id)
            .unwrap()
            .component_ticks
            .get_mut(&retained)
            .unwrap()[location.index_in_archetype]
            .set_changed(changed_tick);
        let retained_before_migration = ticks_for(&world, entity, retained);
        assert_eq!(
            retained_before_migration.added,
            original_retained_ticks.added
        );
        assert_eq!(retained_before_migration.changed, changed_tick);

        let addition_tick = world.increment_change_tick();
        world
            .add_descriptor_component_default(entity, added)
            .unwrap();

        let retained_after_add = ticks_for(&world, entity, retained);
        let removed_after_add = ticks_for(&world, entity, removed);
        let added_after_add = ticks_for(&world, entity, added);
        assert_eq!(retained_after_add.added, retained_before_migration.added);
        assert_eq!(
            retained_after_add.changed,
            retained_before_migration.changed
        );
        assert_eq!(removed_after_add.added, original_removed_ticks.added);
        assert_eq!(removed_after_add.changed, original_removed_ticks.changed);
        assert_eq!(added_after_add.added, addition_tick);
        assert_eq!(added_after_add.changed, addition_tick);

        world.increment_change_tick();
        world.remove_descriptor_component(entity, removed).unwrap();

        let retained_after_remove = ticks_for(&world, entity, retained);
        let added_after_remove = ticks_for(&world, entity, added);
        assert_eq!(retained_after_remove.added, retained_after_add.added);
        assert_eq!(retained_after_remove.changed, retained_after_add.changed);
        assert_eq!(added_after_remove.added, added_after_add.added);
        assert_eq!(added_after_remove.changed, added_after_add.changed);
        assert!(world.descriptor_component_bytes(entity, removed).is_none());
    }

    /// The native byte-chunk accessor exposes a native column's rows as raw
    /// bytes with the correct element size, mirroring the descriptor path used
    /// by the C# backend for optional-module components. Descriptor components
    /// are rejected by it.
    #[test]
    fn native_component_chunk_mut_exposes_raw_rows() {
        let mut world = World::new();
        world.register_component::<Position>();
        let entity = world
            .create_entity()
            .with(Position { x: 1.0, y: 2.0 })
            .build()
            .unwrap();
        let component_id = ComponentId::of::<Position>();
        // Copy the raw facts out while the mutable chunk borrow is still
        // scoped, so the world can be re-borrowed below.
        let (archetype_id, data, len, element_size, ticks_len) = {
            let (archetype_id, data, len, element_size, ticks) = world
                .native_component_chunk_mut(component_id, 0)
                .expect("one archetype column should exist");
            (archetype_id, data, len, element_size, ticks.len())
        };
        assert_eq!(archetype_id, world.entity_locations[&entity].archetype_id);
        assert_eq!(len, 1);
        assert_eq!(element_size, std::mem::size_of::<Position>());
        assert_eq!(ticks_len, 1);
        // SAFETY: `len` is 1 and `element_size` is the Position size, so the
        // returned buffer holds exactly one valid Position.
        let row = unsafe { &*data.cast::<Position>() };
        assert_eq!(row.x, 1.0);
        assert_eq!(row.y, 2.0);

        // Descriptor components are served by the descriptor accessor, not this one.
        let descriptor = world
            .register_component_descriptor(
                0xD1,
                "NativeChunkTest.Descriptor",
                4,
                4,
                1,
                Blittability::engine_verified(),
            )
            .unwrap();
        assert!(world.native_component_chunk_mut(descriptor, 0).is_none());
        // An unknown native id is rejected too.
        let unknown = world
            .register_component_descriptor(
                0xD2,
                "NativeChunkTest.Unknown",
                4,
                4,
                2,
                Blittability::engine_verified(),
            )
            .unwrap();
        assert!(world.native_component_chunk_mut(unknown, 0).is_none());
    }

    /// A byte component adder writes raw ABI bytes into a native column, which
    /// is how the C# backend creates or adds optional-module components whose
    /// concrete Rust type the host never names.
    #[test]
    fn byte_component_adder_writes_native_bytes() {
        use crate::commands::{ByteComponentAdder, CommandQueue, ComponentAdder};

        let mut world = World::new();
        world.register_component::<Position>();
        let entity = world.reserve_entity();
        let component_id = ComponentId::of::<Position>();

        // ABI payload: x = 7.5, y = -3.25, little-endian f32s.
        let mut bytes = Vec::with_capacity(std::mem::size_of::<Position>());
        bytes.extend_from_slice(&7.5_f32.to_ne_bytes());
        bytes.extend_from_slice(&(-3.25_f32).to_ne_bytes());

        let adder = ByteComponentAdder::new(component_id, bytes);
        let mut queue = CommandQueue::new();
        queue.create_mixed_entity(
            entity,
            vec![Box::new(adder) as Box<dyn ComponentAdder>],
            Vec::new(),
        );
        queue.execute_queued_commands(&mut world, true).unwrap();

        let position = world
            .get_component::<Position>(entity)
            .expect("row was created");
        assert_eq!(position.x, 7.5);
        assert_eq!(position.y, -3.25);
    }

    /// The id-keyed sweep releases a column whose factory was purged, empty
    /// or not: the archetype stops listing the id and no column without a
    /// table maker is left behind for a graveyard eviction to invalidate.
    #[test]
    fn a_column_whose_factory_vanished_is_dropped_before_eviction() {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        let component_id = ComponentId::of::<Position>();
        // Two components, so the entity survives the sweep and its destination
        // archetype can be inspected.
        let entity = world
            .create_entity()
            .with(Position { x: 1.0, y: 2.0 })
            .with(Velocity { x: 3.0, y: 4.0 })
            .build()
            .unwrap();

        // What `forget_component_type` leaves when a rebuilt image re-registers
        // the name under a fresh id: the archetype keeps the id and its column,
        // while the factory that describes them is gone.
        world.storage_factories.remove(&component_id);
        assert_eq!(
            world.columns_without_factory(),
            1,
            "the column is the only one without a factory"
        );

        let dropped = world.drop_columns_without_factory();
        assert_eq!(dropped, 1, "the orphaned id was the one swept");
        assert_eq!(
            world.columns_without_factory(),
            0,
            "no column without a function table survives the sweep"
        );
        let location = world.entity_locations[&entity];
        assert!(
            !world.archetypes[&location.archetype_id]
                .component_types
                .contains(&component_id),
            "the entity no longer lists the swept component"
        );
        assert_eq!(
            world.get_component::<Velocity>(entity).unwrap().x,
            3.0,
            "the surviving component kept its value through the migration"
        );
    }

    #[test]
    fn invalid_descriptor_component_layouts_are_rejected() {
        let mut world = World::new();
        assert!(world
            .register_component_descriptor(1, "Zero", 0, 1, 0, Blittability::engine_verified())
            .is_err());
        assert!(world
            .register_component_descriptor(2, "BadAlign", 4, 3, 0, Blittability::engine_verified())
            .is_err());
        assert!(world
            .register_component_descriptor(
                4,
                "Oversized",
                usize::MAX,
                1,
                0,
                Blittability::engine_verified()
            )
            .is_err());
        world
            .register_component_descriptor(3, "SchemaA", 4, 4, 10, Blittability::engine_verified())
            .unwrap();
        assert!(world
            .register_component_descriptor(
                3,
                "SameSchemaDifferentName",
                4,
                4,
                10,
                Blittability::engine_verified()
            )
            .is_err());
        assert!(world
            .register_component_descriptor(3, "SchemaB", 8, 8, 20, Blittability::engine_verified())
            .is_err());

        let valid = world
            .register_component_descriptor(5, "Valid", 4, 4, 30, Blittability::engine_verified())
            .unwrap();
        assert!(world
            .create_descriptor_entity(&[(valid, vec![0; 3])])
            .is_err());
        assert_eq!(world.entity_count(), 0);
    }

    /// Tests creating multiple entities with different component combinations.
    ///
    /// This test verifies that:
    /// - Entities can be created with various combinations of components
    /// - Each unique component combination creates a separate archetype
    /// - All created entities are properly tracked in the world
    ///
    /// Expected results:
    /// - 3 entities should be created in total
    /// - 3 different archetypes should exist (Position+Velocity, Position, Position+Velocity+Health)
    /// - All entity IDs should be present in the entity_locations map
    #[test]
    fn test_create_entities_with_different_components() {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world.register_component::<Health>();

        // Create entity with Position + Velocity
        let entity1 = world
            .create_entity()
            .with(Position { x: 10.0, y: 20.0 })
            .with(Velocity { x: 1.0, y: 2.0 })
            .build()
            .unwrap();

        // Create entity with Position only
        let entity2 = world
            .create_entity()
            .with(Position { x: 5.0, y: 15.0 })
            .build()
            .unwrap();

        // Create entity with all three components
        let entity3 = world
            .create_entity()
            .with(Position { x: 100.0, y: 200.0 })
            .with(Velocity { x: 5.0, y: 10.0 })
            .with(Health { hp: 100 })
            .build()
            .unwrap();

        assert_eq!(world.entity_locations.len(), 3);
        assert_eq!(world.archetypes.len(), 3);
        assert!(world.entity_locations.contains_key(&entity1));
        assert!(world.entity_locations.contains_key(&entity2));
        assert!(world.entity_locations.contains_key(&entity3));

        // Print archetype information
        world.print_archetypes();

        // Verify each archetype's component mask matches expected components
        for (archetype_id, archetype) in world.archetypes.iter() {
            println!("\n--- Verifying Archetype {:?} ---", archetype_id);

            // Get component names
            let comp_names: Vec<String> = archetype
                .component_types
                .iter()
                .filter_map(|component_id| {
                    world
                        .component_registry
                        .get_name(component_id)
                        .map(String::from)
                })
                .collect();

            println!("Components: {:?}", comp_names);

            // Build expected mask from component types
            let mut expected_mask = ComponentMask::empty();
            for component_id in &archetype.component_types {
                if let Some(bit) = world.component_registry.get_bit(component_id) {
                    expected_mask.set(bit);
                    println!(
                        "  - {:?} -> bit {}",
                        world
                            .component_registry
                            .get_name(component_id)
                            .unwrap_or("Unknown"),
                        bit
                    );
                }
            }

            // Verify masks match
            assert_eq!(
                archetype.component_mask, expected_mask,
                "Archetype {:?} mask mismatch!\nActual:   {:?}\nExpected: {:?}",
                archetype_id, archetype.component_mask, expected_mask
            );

            println!("✓ Mask verified: {:?}", archetype.component_mask);
        }

        println!("\n✓ All 3 archetypes verified successfully!");
    }

    /// Tests adding a new component to an existing entity.
    ///
    /// This test verifies that:
    /// - A component can be added to an entity that doesn't already have it
    /// - The entity is migrated to a new archetype with the added component
    /// - Existing components on the entity are preserved during migration
    /// - The entity remains valid and tracked in the world
    /// - The old archetype is automatically cleaned up when it becomes empty
    ///
    /// Expected results:
    /// - add_component should return true (success)
    /// - The entity should still exist in entity_locations
    /// - Old archetype should be automatically removed, leaving 1 archetype
    #[test]
    fn test_add_component_to_entity() {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world.register_component::<Health>();

        let entity = world
            .create_entity()
            .with(Position { x: 10.0, y: 20.0 })
            .with(Velocity { x: 1.0, y: 2.0 })
            .build()
            .unwrap();

        assert_eq!(
            world.archetypes.len(),
            1,
            "Should have 1 archetype initially"
        );

        // Add Health component
        let result = world.add_component(entity, Health { hp: 50 });

        assert!(result.is_ok(), "Should successfully add component");
        assert!(world.entity_locations.contains_key(&entity));

        // Since this is the only entity, the old archetype should be automatically removed
        assert_eq!(
            world.archetypes.len(),
            1,
            "Should have 1 archetype after adding Health (old one auto-removed)"
        );

        world.print_archetypes();
    }

    /// Tests attempting to add a component to a non-existent entity.
    ///
    /// This test verifies that:
    /// - The system handles invalid entity IDs gracefully
    /// - No panic or crash occurs when operating on a fake entity
    /// - The operation correctly returns failure status
    ///
    /// Expected results:
    /// - add_component should return false (failure)
    /// - No side effects or modifications to the world state
    #[test]
    fn test_add_component_to_nonexistent_entity() {
        let mut world = World::new();
        world.register_component::<Position>();

        let fake_entity = crate::Entity::new_for_test(9999, 0);
        let result = world.add_component(fake_entity, Position { x: 0.0, y: 0.0 });

        assert_eq!(
            result,
            Err(AddComponentError::EntityNotFound),
            "Should fail to add component to non-existent entity"
        );
    }

    /// Tests removing a component from an entity that has multiple components.
    ///
    /// This test verifies that:
    /// - A specific component can be removed from an entity
    /// - The entity is migrated to a new archetype without the removed component
    /// - Other components remain intact on the entity
    /// - The entity continues to exist in the world
    ///
    /// Expected results:
    /// - remove_component should return true (success)
    /// - The entity should still be tracked in entity_locations
    /// - The entity should be in a different archetype (Position+Health instead of Position+Velocity+Health)
    #[test]
    fn test_remove_component_from_entity() {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world.register_component::<Health>();

        let entity = world
            .create_entity()
            .with(Position { x: 100.0, y: 200.0 })
            .with(Velocity { x: 5.0, y: 10.0 })
            .with(Health { hp: 100 })
            .build()
            .unwrap();

        // Remove Velocity component
        let result = world.remove_component::<Velocity>(entity);

        assert_eq!(world.archetypes.len(), 1, "Should have 1 archetype");

        assert!(result.is_ok(), "Should successfully remove component");
        assert!(world.entity_locations.contains_key(&entity));

        let location = world.entity_locations.get(&entity).unwrap();
        let archetype = world.archetypes.get(&location.archetype_id).unwrap();

        // Archetype should now only have Position and Health
        assert_eq!(
            archetype.component_types.len(),
            2,
            "Should have 2 component types"
        );

        // Archetype should contain Position and Health, but not Velocity. Checking component IDs.
        // Verify component IDs are as expected
        let position_id = ComponentId::of::<Position>();
        let health_id = ComponentId::of::<Health>();
        let velocity_id = ComponentId::of::<Velocity>();

        assert!(
            archetype.component_types.contains(&position_id),
            "Archetype should contain Position component"
        );
        assert!(
            archetype.component_types.contains(&health_id),
            "Archetype should contain Health component"
        );
        assert!(
            !archetype.component_types.contains(&velocity_id),
            "Archetype should not contain Velocity component"
        );
    }

    /// Tests attempting to remove a component from a non-existent entity.
    ///
    /// This test verifies that:
    /// - The system handles invalid entity IDs gracefully during removal
    /// - No panic occurs when trying to remove from a fake entity
    /// - The operation correctly reports failure
    ///
    /// Expected results:
    /// - remove_component should return false (failure)
    /// - No modifications to the world state
    #[test]
    fn test_remove_component_from_nonexistent_entity() {
        let mut world = World::new();
        world.register_component::<Velocity>();

        let fake_entity = crate::Entity::new_for_test(9999, 0);
        let result = world.remove_component::<Velocity>(fake_entity);

        assert_eq!(
            result,
            Err(RemoveComponentError::EntityNotFound),
            "Should fail to remove component from non-existent entity"
        );
    }

    /// Tests removing the last component from an entity, which should destroy it.
    ///
    /// This test verifies that:
    /// - When an entity's last component is removed, the entity is automatically destroyed
    /// - No entities with zero components are left in the world
    /// - The entity is properly removed from all tracking structures
    /// - If entity count drops to zero, archetypes are cleaned up
    ///
    /// Expected results:
    /// - remove_component should return true (success)
    /// - The entity count should drop to 0
    /// - The entity should no longer exist in entity_locations
    /// - All archetypes should be removed if no entities remain
    #[test]
    fn test_remove_last_component_destroys_entity() {
        let mut world = World::new();
        world.register_component::<Position>();

        let entity = world
            .create_entity()
            .with(Position { x: 5.0, y: 15.0 })
            .build()
            .unwrap();

        assert_eq!(world.entity_locations.len(), 1);

        // Remove the only component - should destroy entity
        let result = world.remove_component::<Position>(entity);

        assert!(result.is_ok(), "Should successfully remove component");
        assert_eq!(
            world.entity_locations.len(),
            0,
            "Entity should be destroyed"
        );
        assert!(!world.entity_locations.contains_key(&entity));

        assert!(world.archetypes.is_empty(), "No archetypes should remain");
    }

    /// Tests destroying an entity and verifying other entities remain unaffected.
    ///
    /// This test verifies that:
    /// - An entity can be completely removed from the world
    /// - Destroying one entity doesn't affect other entities
    /// - The entity is removed from its archetype and all tracking structures
    /// - The total entity count decreases correctly
    ///
    /// Expected results:
    /// - destroy should return true (success)
    /// - Entity count should decrease from 2 to 1
    /// - The destroyed entity should no longer exist in entity_locations
    /// - The other entity should remain unaffected
    #[test]
    fn test_destroy_entity() {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();

        let entity1 = world
            .create_entity()
            .with(Position { x: 10.0, y: 20.0 })
            .build()
            .unwrap();

        let entity2 = world
            .create_entity()
            .with(Position { x: 5.0, y: 15.0 })
            .with(Velocity { x: 1.0, y: 2.0 })
            .build()
            .unwrap();

        assert_eq!(world.entity_locations.len(), 2);

        // Destroy entity1
        let result = world.destroy_entity(entity1);

        assert!(result, "Should successfully destroy entity");
        assert_eq!(world.entity_locations.len(), 1);
        assert!(!world.entity_locations.contains_key(&entity1));
        assert!(world.entity_locations.contains_key(&entity2));
    }

    /// Tests attempting to destroy a non-existent entity.
    ///
    /// This test verifies that:
    /// - The system handles invalid entity IDs gracefully during destroy
    /// - No panic or crash occurs when destroying a fake entity
    /// - The operation correctly reports failure
    ///
    /// Expected results:
    /// - destroy should return false (failure)
    /// - No changes to the world state
    #[test]
    fn test_destroy_nonexistent_entity() {
        let mut world = World::new();
        let fake_entity = crate::Entity::new_for_test(9999, 0);

        let result = world.destroy_entity(fake_entity);

        assert!(!result, "Should fail to destroy non-existent entity");
    }

    /// Tests that attempting to destroy an already-destroyed entity fails correctly.
    ///
    /// This test verifies that:
    /// - Once an entity is destroyed, it cannot be destroyed again
    /// - The system properly tracks which entities exist vs don't exist
    /// - Repeated destroy operations are safely rejected
    ///
    /// Expected results:
    /// - First destroy should return true (success)
    /// - Second destroy should return false (entity no longer exists)
    /// - No panic or invalid state from double-destroy attempt
    #[test]
    fn test_destroy_already_destroyed_entity() {
        let mut world = World::new();
        world.register_component::<Position>();

        let entity = world
            .create_entity()
            .with(Position { x: 10.0, y: 20.0 })
            .build()
            .unwrap();

        // First destroy should succeed
        let result1 = world.destroy_entity(entity);
        assert!(result1);

        // Second destroy should fail
        let result2 = world.destroy_entity(entity);
        assert!(!result2, "Should fail to destroy already-destroyed entity");
    }

    /// Tests the cleanup of empty archetypes after entities are destroyed.
    ///
    /// This test verifies that:
    /// - When all entities are removed from an archetype, it becomes empty
    /// - The cleanup_empty_archetypes method removes unused archetypes
    /// - Non-empty archetypes and their entities remain unaffected
    /// - Memory is properly reclaimed from empty archetype storage
    ///
    /// Expected results:
    /// - Initially 2 archetypes should exist
    /// - After destroying entity1 and cleanup, archetype count should decrease
    /// - entity2 should still exist and be properly tracked
    #[test]
    fn test_cleanup_empty_archetypes() {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();

        // Create some entities
        let entity1 = world
            .create_entity()
            .with(Position { x: 10.0, y: 20.0 })
            .build()
            .unwrap();

        let entity2 = world
            .create_entity()
            .with(Position { x: 5.0, y: 15.0 })
            .with(Velocity { x: 1.0, y: 2.0 })
            .build()
            .unwrap();

        let initial_archetypes = world.archetypes.len();
        assert_eq!(initial_archetypes, 2);

        // Destroy one entity, leaving one archetype empty
        let _ = world.destroy_entity(entity1);

        // Cleanup should remove empty archetype
        world.cleanup_empty_archetypes();

        assert!(world.archetypes.len() < initial_archetypes);
        assert!(world.entity_locations.contains_key(&entity2));
    }

    /// Tests entity migration between archetypes when components are added and removed.
    ///
    /// This test verifies that:
    /// - Adding a component moves the entity to a different archetype
    /// - Removing a component moves the entity to yet another archetype
    /// - Each archetype change is properly tracked with different archetype IDs
    /// - Component data is preserved during migrations
    ///
    /// Expected results:
    /// - Entity starts in archetype for (Position+Velocity)
    /// - After adding Health, entity moves to archetype for (Position+Velocity+Health)
    /// - After removing Velocity, entity moves to archetype for (Position+Health)
    /// - All three archetype IDs should be different from each other
    #[test]
    fn test_entity_archetype_migration() {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world.register_component::<Health>();

        // Start with Position + Velocity
        let entity = world
            .create_entity()
            .with(Position { x: 10.0, y: 20.0 })
            .with(Velocity { x: 1.0, y: 2.0 })
            .build()
            .unwrap();

        let initial_location = *world.entity_locations.get(&entity).unwrap();

        // Add Health - should migrate to new archetype
        world.add_component(entity, Health { hp: 100 }).unwrap();

        let after_add_location = *world.entity_locations.get(&entity).unwrap();
        assert_ne!(
            initial_location.archetype_id, after_add_location.archetype_id,
            "Entity should be in different archetype after adding component"
        );

        // Remove Velocity - should migrate to another archetype
        world.remove_component::<Velocity>(entity).unwrap();

        let after_remove_location = *world.entity_locations.get(&entity).unwrap();
        assert_ne!(
            after_add_location.archetype_id, after_remove_location.archetype_id,
            "Entity should be in different archetype after removing component"
        );
    }

    /// Tests that empty archetypes are automatically cleaned up when last entity moves.
    ///
    /// This test verifies that:
    /// - When the last entity in an archetype is moved to another archetype, the empty one is removed
    /// - The archetype is removed from both the archetypes map and the lookup table
    /// - No manual cleanup_empty_archetypes() call is needed
    /// - The world remains in a consistent state
    ///
    /// Expected results:
    /// - Initially 1 archetype exists (Position+Velocity)
    /// - After adding Health, 2 archetypes exist temporarily
    /// - The old archetype is automatically removed, leaving only 1 archetype
    /// - The entity is correctly tracked in the new archetype
    #[test]
    fn test_automatic_empty_archetype_cleanup() {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world.register_component::<Health>();

        // Create single entity with Position + Velocity
        let entity = world
            .create_entity()
            .with(Position { x: 10.0, y: 20.0 })
            .with(Velocity { x: 1.0, y: 2.0 })
            .build()
            .unwrap();

        assert_eq!(
            world.archetypes.len(),
            1,
            "Should have 1 archetype initially"
        );

        // Add Health - this should move entity to new archetype
        // The old archetype should be automatically removed since it becomes empty
        world.add_component(entity, Health { hp: 100 }).unwrap();

        assert_eq!(
            world.archetypes.len(),
            1,
            "Should still have 1 archetype after migration (old one auto-removed)"
        );
        assert!(world.entity_locations.contains_key(&entity));

        // Verify the entity is in the correct archetype with all 3 components
        let location = world.entity_locations.get(&entity).unwrap();
        let archetype = world.archetypes.get(&location.archetype_id).unwrap();
        assert_eq!(
            archetype.component_types.len(),
            3,
            "Entity should have 3 components"
        );

        println!("✓ Empty archetype automatically cleaned up after entity migration");
    }

    /// Test that archetype print_info displays component names and entity count
    #[test]
    fn test_archetype_print_info() {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();

        // Create some entities
        world
            .create_entity()
            .with(Position { x: 10.0, y: 20.0 })
            .with(Velocity { x: 1.0, y: 2.0 })
            .build()
            .unwrap();

        world
            .create_entity()
            .with(Position { x: 5.0, y: 15.0 })
            .with(Velocity { x: 0.5, y: 1.5 })
            .build()
            .unwrap();

        // Print info using the world helper method
        world.print_archetypes();

        // Verify archetype structure
        assert_eq!(world.entity_locations.len(), 2, "Should have 2 entities");
        assert_eq!(world.archetypes.len(), 1, "Should have 1 archetype");

        // Get the archetype and verify its contents
        let archetype = world.archetypes.values().next().unwrap();
        assert_eq!(
            archetype.entities.len(),
            2,
            "Archetype should contain 2 entities"
        );
        assert_eq!(
            archetype.component_types.len(),
            2,
            "Archetype should have 2 component types"
        );

        // Verify component names are registered and retrievable
        let comp_names: Vec<String> = archetype
            .component_types
            .iter()
            .filter_map(|component_id| {
                world
                    .component_registry
                    .get_name(component_id)
                    .map(String::from)
            })
            .collect();

        assert_eq!(comp_names.len(), 2, "Should have 2 component names");

        // Check that both expected component names are present
        let has_position = comp_names.iter().any(|name| name.contains("Position"));
        let has_velocity = comp_names.iter().any(|name| name.contains("Velocity"));

        assert!(
            has_position,
            "Should contain Position component, found: {:?}",
            comp_names
        );
        assert!(
            has_velocity,
            "Should contain Velocity component, found: {:?}",
            comp_names
        );

        println!("✓ Component names verified: {:?}", comp_names);
    }

    /// Tests entity generation system for safe ID recycling.
    ///
    /// This test verifies that:
    /// - Entity IDs are recycled after destruction
    /// - Generations are incremented when IDs are reused
    /// - Stale handles (old generation) cannot access recycled entities
    /// - New entities with recycled IDs work correctly
    ///
    /// Expected results:
    /// - Destroyed entity's ID should be reused for new entity
    /// - New entity should have same ID but different generation
    /// - Old handle should be invalid (is_entity_valid returns false)
    /// - Old handle should not access new entity's components
    #[test]
    fn test_entity_generations() {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();

        // Create first entity
        let entity1 = world
            .create_entity()
            .with(Position { x: 10.0, y: 20.0 })
            .build()
            .unwrap();

        println!("Entity1: id={}, gen={}", entity1.id, entity1.generation);
        assert_eq!(entity1.id, 0, "First entity should have ID 0");
        assert_eq!(
            entity1.generation, 0,
            "First entity should have generation 0"
        );

        // Verify entity1 exists and has component
        assert!(world.is_entity_valid(entity1), "Entity1 should be valid");
        assert!(
            world.get_component::<Position>(entity1).is_some(),
            "Entity1 should have Position"
        );

        // Destroy entity1
        let destroyed = world.destroy_entity(entity1);
        assert!(destroyed, "Entity1 should be destroyed successfully");

        // Verify entity1 is no longer valid
        assert!(
            !world.is_entity_valid(entity1),
            "Entity1 should be invalid after destruction"
        );
        assert!(
            world.get_component::<Position>(entity1).is_none(),
            "Destroyed entity should not have components"
        );

        // Create a new entity - should reuse ID 0 with generation 1
        let entity2 = world
            .create_entity()
            .with(Velocity { x: 5.0, y: 10.0 })
            .build()
            .unwrap();

        println!("Entity2: id={}, gen={}", entity2.id, entity2.generation);
        assert_eq!(entity2.id, 0, "New entity should reuse ID 0");
        assert_eq!(entity2.generation, 1, "New entity should have generation 1");

        // Verify entity2 is valid
        assert!(world.is_entity_valid(entity2), "Entity2 should be valid");
        assert!(
            world.get_component::<Velocity>(entity2).is_some(),
            "Entity2 should have Velocity"
        );

        // Critical: Old handle (entity1) should NOT access entity2's data
        assert!(
            !world.is_entity_valid(entity1),
            "Old handle should still be invalid"
        );
        assert!(
            world.get_component::<Velocity>(entity1).is_none(),
            "Old handle should not access new entity's components"
        );
        assert!(
            world.get_component::<Position>(entity1).is_none(),
            "Old handle should not access any components"
        );

        // Verify they are different entities (different hash/eq)
        assert_ne!(
            entity1, entity2,
            "Entities with different generations should not be equal"
        );

        println!("✓ Entity generation recycling works correctly!");
    }

    /// Tests multiple rounds of entity recycling.
    ///
    /// This test verifies that:
    /// - Multiple destroy/create cycles correctly increment generations
    /// - The free list works correctly with multiple recycled IDs
    /// - Generations wrap around safely (using wrapping_add)
    #[test]
    fn test_multiple_entity_recycling_rounds() {
        let mut world = World::new();
        world.register_component::<Position>();

        // Create and destroy the same ID multiple times
        let mut last_entity = world
            .create_entity()
            .with(Position { x: 0.0, y: 0.0 })
            .build()
            .unwrap();
        assert_eq!(last_entity.id, 0);
        assert_eq!(last_entity.generation, 0);

        for round in 1..=5 {
            let old_entity = last_entity;
            let _ = world.destroy_entity(old_entity);

            let new_entity = world
                .create_entity()
                .with(Position {
                    x: round as f32,
                    y: 0.0,
                })
                .build()
                .unwrap();

            assert_eq!(new_entity.id, 0, "Should reuse ID 0 in round {}", round);
            assert_eq!(
                new_entity.generation, round,
                "Generation should be {} in round {}",
                round, round
            );

            // Old handle should be invalid
            assert!(!world.is_entity_valid(old_entity));
            // New handle should be valid
            assert!(world.is_entity_valid(new_entity));

            last_entity = new_entity;
        }

        println!("✓ Multiple recycling rounds work correctly!");
    }

    /// Tests that multiple entities can be recycled independently.
    ///
    /// This test verifies LIFO (stack) behavior of the free list.
    #[test]
    fn test_free_list_lifo_order() {
        let mut world = World::new();
        world.register_component::<Position>();

        // Create 3 entities
        let entity0 = world
            .create_entity()
            .with(Position { x: 0.0, y: 0.0 })
            .build()
            .unwrap();
        let entity1 = world
            .create_entity()
            .with(Position { x: 1.0, y: 1.0 })
            .build()
            .unwrap();
        let entity2 = world
            .create_entity()
            .with(Position { x: 2.0, y: 2.0 })
            .build()
            .unwrap();

        assert_eq!(entity0.id, 0);
        assert_eq!(entity1.id, 1);
        assert_eq!(entity2.id, 2);

        // Destroy in order: entity0, entity1, entity2
        let _ = world.destroy_entity(entity0);
        let _ = world.destroy_entity(entity1);
        let _ = world.destroy_entity(entity2);

        // Free list should be: [(0, 1), (1, 1), (2, 1)]
        // Pop order (LIFO): entity2's ID first, then entity1's, then entity0's

        let new_entity1 = world
            .create_entity()
            .with(Position { x: 0.0, y: 0.0 })
            .build()
            .unwrap();
        assert_eq!(new_entity1.id, 2, "Should pop ID 2 first (LIFO)");
        assert_eq!(new_entity1.generation, 1);

        let new_entity2 = world
            .create_entity()
            .with(Position { x: 0.0, y: 0.0 })
            .build()
            .unwrap();
        assert_eq!(new_entity2.id, 1, "Should pop ID 1 second");
        assert_eq!(new_entity2.generation, 1);

        let new_entity3 = world
            .create_entity()
            .with(Position { x: 0.0, y: 0.0 })
            .build()
            .unwrap();
        assert_eq!(new_entity3.id, 0, "Should pop ID 0 third");
        assert_eq!(new_entity3.generation, 1);

        // Next entity should get a fresh ID
        let new_entity4 = world
            .create_entity()
            .with(Position { x: 0.0, y: 0.0 })
            .build()
            .unwrap();
        assert_eq!(new_entity4.id, 3, "Should allocate fresh ID 3");
        assert_eq!(new_entity4.generation, 0);

        println!("✓ Free list LIFO order works correctly!");
    }

    /// A slot whose generation reaches `u32::MAX` is retired instead of
    /// wrapping back to zero (audit 5.14 / 4.1).
    ///
    /// Wrapping would resurrect every stale handle from 2^32 recycles ago -
    /// the ABA failure where a handle silently addresses an unrelated entity.
    /// The free list must drop the slot instead, so a stale handle can never
    /// validate against a recycled entity.
    #[test]
    fn slot_at_generation_max_is_retired_not_wrapped() {
        let mut world = World::new();
        world.register_component::<Position>();

        // Seed the free list so the next create reuses id 7 at the
        // second-highest representable generation.
        world.free_entity_ids.push((7, u32::MAX - 1));

        // First life of the slot: a real entity near the ceiling.
        let stale = world
            .create_entity()
            .with(Position { x: 0.0, y: 0.0 })
            .build()
            .unwrap();
        assert_eq!(stale.id, 7);
        assert_eq!(stale.generation, u32::MAX - 1);

        // Destroying it recycles the slot one step closer to the ceiling.
        assert!(world.destroy_entity(stale));
        assert_eq!(world.free_entity_ids.as_slice(), &[(7, u32::MAX)]);

        // Second life at the ceiling itself.
        let ceiling = world
            .create_entity()
            .with(Position { x: 0.0, y: 0.0 })
            .build()
            .unwrap();
        assert_eq!(ceiling.generation, u32::MAX);

        // Destroying at the ceiling retires the slot instead of wrapping to 0.
        assert!(world.destroy_entity(ceiling));
        assert!(
            world.free_entity_ids.is_empty(),
            "the slot must be retired, not wrapped back to generation 0"
        );

        // The id is never handed out again, so the stale handle can never
        // validate against a recycled entity.
        let replacement = world
            .create_entity()
            .with(Position { x: 0.0, y: 0.0 })
            .build()
            .unwrap();
        assert_ne!(replacement.id, 7, "a retired slot must not be recycled");
        assert!(!world.is_entity_valid(stale), "the stale handle stays dead");
    }

    /// Tests entity generations with multiple archetypes and component removal.
    ///
    /// This test verifies that:
    /// - Entities in different archetypes have independent generation tracking
    /// - Removing components (which moves entity to new archetype) preserves entity identity
    /// - Destroying entities from different archetypes correctly adds IDs to free list
    /// - Recycled IDs work correctly regardless of which archetype the original was in
    #[test]
    fn test_generations_with_multiple_archetypes_and_component_removal() {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world.register_component::<Health>();

        // Create 3 entities:
        // entity1, entity2: Position + Velocity (same archetype)
        // entity3: Position + Health (different archetype)
        let entity1 = world
            .create_entity()
            .with(Position { x: 1.0, y: 1.0 })
            .with(Velocity { x: 10.0, y: 10.0 })
            .build()
            .unwrap();

        let entity2 = world
            .create_entity()
            .with(Position { x: 2.0, y: 2.0 })
            .with(Velocity { x: 20.0, y: 20.0 })
            .build()
            .unwrap();

        let entity3 = world
            .create_entity()
            .with(Position { x: 3.0, y: 3.0 })
            .with(Health { hp: 100 })
            .build()
            .unwrap();

        println!(
            "Created: entity1(id={}, gen={}), entity2(id={}, gen={}), entity3(id={}, gen={})",
            entity1.id,
            entity1.generation,
            entity2.id,
            entity2.generation,
            entity3.id,
            entity3.generation
        );

        assert_eq!(entity1.id, 0);
        assert_eq!(entity2.id, 1);
        assert_eq!(entity3.id, 2);
        assert_eq!(world.archetypes.len(), 2, "Should have 2 archetypes");

        // Remove Velocity from entity1 - moves it to Position-only archetype
        let old_entity1 = entity1;
        let removed = world.remove_component::<Velocity>(entity1);
        assert!(removed.is_ok(), "Should remove Velocity from entity1");

        // entity1 should still be valid with same id and generation (entity wasn't destroyed)
        assert!(
            world.is_entity_valid(entity1),
            "entity1 should still be valid after component removal"
        );
        assert!(
            world.get_component::<Position>(entity1).is_some(),
            "entity1 should still have Position"
        );
        assert!(
            world.get_component::<Velocity>(entity1).is_none(),
            "entity1 should not have Velocity"
        );

        // Destroy entity2 (from Position+Velocity archetype)
        let old_entity2 = entity2;
        let _ = world.destroy_entity(entity2);
        assert!(
            !world.is_entity_valid(old_entity2),
            "entity2 should be invalid after destruction"
        );

        // Destroy entity3 (from Position+Health archetype)
        let old_entity3 = entity3;
        let _ = world.destroy_entity(entity3);
        assert!(
            !world.is_entity_valid(old_entity3),
            "entity3 should be invalid after destruction"
        );

        // Free list should now have: [(1, 1), (2, 1)] (LIFO order)
        // entity1 (id=0) is still alive

        // Create new entity - should reuse ID 2 (last destroyed)
        let new_entity1 = world
            .create_entity()
            .with(Health { hp: 50 })
            .build()
            .unwrap();

        println!(
            "new_entity1: id={}, gen={}",
            new_entity1.id, new_entity1.generation
        );
        assert_eq!(new_entity1.id, 2, "Should reuse ID 2 (LIFO)");
        assert_eq!(new_entity1.generation, 1, "Should have generation 1");

        // Old entity3 handle should NOT access new_entity1's data
        assert!(
            !world.is_entity_valid(old_entity3),
            "Old entity3 handle should be invalid"
        );
        assert!(
            world.get_component::<Health>(old_entity3).is_none(),
            "Old handle should not access new entity"
        );

        // Create another entity - should reuse ID 1
        let new_entity2 = world
            .create_entity()
            .with(Position { x: 0.0, y: 0.0 })
            .with(Velocity { x: 0.0, y: 0.0 })
            .build()
            .unwrap();

        println!(
            "new_entity2: id={}, gen={}",
            new_entity2.id, new_entity2.generation
        );
        assert_eq!(new_entity2.id, 1, "Should reuse ID 2");
        assert_eq!(new_entity2.generation, 1, "Should have generation 1");

        // Old entity2 handle should NOT access new_entity2's data
        assert!(
            !world.is_entity_valid(old_entity2),
            "Old entity2 handle should be invalid"
        );

        // Verify entity1 (never destroyed) still works with original handle
        assert!(
            world.is_entity_valid(old_entity1),
            "Original entity1 should still be valid"
        );
        let pos = world.get_component::<Position>(old_entity1).unwrap();
        assert_eq!(pos.x, 1.0, "entity1 Position should be preserved");

        // Destroy entity1 and verify recycling
        let _ = world.destroy_entity(entity1);
        assert!(
            !world.is_entity_valid(old_entity1),
            "entity1 should be invalid after destruction"
        );

        let new_entity3 = world
            .create_entity()
            .with(Position { x: 0.0, y: 0.0 })
            .build()
            .unwrap();
        println!(
            "new_entity3: id={}, gen={}",
            new_entity3.id, new_entity3.generation
        );
        assert_eq!(new_entity3.id, 0, "Should reuse ID 1");
        assert_eq!(new_entity3.generation, 1, "Should have generation 1");

        println!("✓ Generations with multiple archetypes and component removal work correctly!");
    }

    /// Tests that component data is correctly swap-removed when an entity is destroyed.
    ///
    /// This test exposes the bug where component data is NOT swap-removed from storage
    /// when an entity is destroyed, causing remaining entities to read stale/wrong data.
    ///
    /// Expected behavior
    /// - After destroying entity0, entity2 should still have its original Position (2.0, 2.0)
    /// - Currently, entity2 reads entity0's old Position (0.0, 0.0) - BUG!
    #[test]
    fn test_component_swap_remove_on_destroy() {
        let mut world = World::new();
        world.register_component::<Position>();

        // Create 3 entities in the same archetype
        let entity0 = world
            .create_entity()
            .with(Position { x: 0.0, y: 0.0 })
            .build()
            .unwrap();
        let entity1 = world
            .create_entity()
            .with(Position { x: 1.0, y: 1.0 })
            .build()
            .unwrap();
        let entity2 = world
            .create_entity()
            .with(Position { x: 2.0, y: 2.0 })
            .build()
            .unwrap();

        // Verify initial state
        assert_eq!(world.get_component::<Position>(entity0).unwrap().x, 0.0);
        assert_eq!(world.get_component::<Position>(entity1).unwrap().x, 1.0);
        assert_eq!(world.get_component::<Position>(entity2).unwrap().x, 2.0);

        // Archetype entity list: [entity0, entity1, entity2] (indices 0, 1, 2)
        // Component storage:     [Pos(0,0), Pos(1,1), Pos(2,2)]

        // Destroy entity0 (index 0)
        // Entity list swap_remove: entity2 moves from index 2 to index 0
        // Entity list becomes: [entity2, entity1] (entity2 now at index 0)
        //
        // BUG: Component storage is NOT updated!
        // Component storage still: [Pos(0,0), Pos(1,1), Pos(2,2)]
        //
        // Now entity2 has index 0, but component at index 0 is Pos(0,0) - WRONG!
        let _ = world.destroy_entity(entity0);

        // entity1 should still have its original position (index 1 unchanged)
        let pos1 = world.get_component::<Position>(entity1).unwrap();
        assert_eq!(pos1.x, 1.0, "entity1 Position.x should be 1.0");
        assert_eq!(pos1.y, 1.0, "entity1 Position.y should be 1.0");

        // entity2 was swapped to index 0 - it should still have Position(2.0, 2.0)
        // BUG: It actually reads Position(0.0, 0.0) because component storage wasn't swap-removed
        let pos2 = world.get_component::<Position>(entity2).unwrap();

        println!(
            "entity2 Position after entity0 destroyed: ({}, {})",
            pos2.x, pos2.y
        );
        println!("Expected: (2.0, 2.0), Got: ({}, {})", pos2.x, pos2.y);

        // This assertion FAILS because of the unimplemented component swap_remove
        assert_eq!(
            pos2.x, 2.0,
            "BUG: entity2 should have Position.x = 2.0, but got {} (entity0's old data)",
            pos2.x
        );
        assert_eq!(
            pos2.y, 2.0,
            "BUG: entity2 should have Position.y = 2.0, but got {} (entity0's old data)",
            pos2.y
        );

        println!("✓ Component swap_remove works correctly!");
    }

    /// Tests that component data is properly cleaned up when an entity migrates between archetypes.
    ///
    /// When an entity gains or loses a component, it moves to a different archetype.
    /// The old archetype must properly remove the entity's component data using swap_remove.
    /// Otherwise:
    /// 1. Memory leaks occur (orphaned component data)
    /// 2. Other entities in the old archetype may read wrong component data
    ///
    /// This test verifies:
    /// - Component data is removed from old archetype during migration
    /// - Other entities in the old archetype still have correct component data
    /// - The swapped entity (if any) correctly maps to its swapped component data
    #[test]
    fn test_component_cleanup_on_archetype_migration() {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        world.register_component::<Health>();

        // Create 3 entities with Position + Velocity in the same archetype
        let entity0 = world
            .create_entity()
            .with(Position { x: 0.0, y: 0.0 })
            .with(Velocity { x: 100.0, y: 100.0 })
            .build()
            .unwrap();
        let entity1 = world
            .create_entity()
            .with(Position { x: 1.0, y: 1.0 })
            .with(Velocity { x: 101.0, y: 101.0 })
            .build()
            .unwrap();
        let entity2 = world
            .create_entity()
            .with(Position { x: 2.0, y: 2.0 })
            .with(Velocity { x: 102.0, y: 102.0 })
            .build()
            .unwrap();

        // Verify initial state
        assert_eq!(
            world.archetypes.len(),
            1,
            "Should have 1 archetype initially"
        );

        // Verify all entities have correct data
        assert_eq!(world.get_component::<Position>(entity0).unwrap().x, 0.0);
        assert_eq!(world.get_component::<Velocity>(entity0).unwrap().x, 100.0);
        assert_eq!(world.get_component::<Position>(entity1).unwrap().x, 1.0);
        assert_eq!(world.get_component::<Velocity>(entity1).unwrap().x, 101.0);
        assert_eq!(world.get_component::<Position>(entity2).unwrap().x, 2.0);
        assert_eq!(world.get_component::<Velocity>(entity2).unwrap().x, 102.0);

        // Now add Health to entity0 - this moves it to a NEW archetype (Position+Velocity+Health)
        // The old archetype (Position+Velocity) should swap_remove entity0's data
        // entity2 should be swapped into index 0
        world.add_component(entity0, Health { hp: 50 }).unwrap();

        // Verify entity0 moved to new archetype and has all components
        assert!(world.get_component::<Position>(entity0).is_some());
        assert!(world.get_component::<Velocity>(entity0).is_some());
        assert!(world.get_component::<Health>(entity0).is_some());
        assert_eq!(world.get_component::<Position>(entity0).unwrap().x, 0.0);
        assert_eq!(world.get_component::<Velocity>(entity0).unwrap().x, 100.0);
        assert_eq!(world.get_component::<Health>(entity0).unwrap().hp, 50);

        // CRITICAL: entity1 and entity2 should still have correct data in old archetype
        // If swap_remove wasn't applied to component storage, entity2 (now at index 0)
        // would incorrectly read entity0's old data!

        let pos1 = world.get_component::<Position>(entity1).unwrap();
        let vel1 = world.get_component::<Velocity>(entity1).unwrap();
        assert_eq!(pos1.x, 1.0, "entity1 Position.x should be 1.0");
        assert_eq!(vel1.x, 101.0, "entity1 Velocity.x should be 101.0");

        let pos2 = world.get_component::<Position>(entity2).unwrap();
        let vel2 = world.get_component::<Velocity>(entity2).unwrap();
        assert_eq!(
            pos2.x, 2.0,
            "entity2 Position.x should be 2.0, but got {} (possible swap_remove bug)",
            pos2.x
        );
        assert_eq!(
            vel2.x, 102.0,
            "entity2 Velocity.x should be 102.0, but got {} (possible swap_remove bug)",
            vel2.x
        );

        // Verify archetype count (old one should still exist with entity1, entity2)
        assert_eq!(world.archetypes.len(), 2, "Should have 2 archetypes now");

        // Now remove Velocity from entity1 - moves to Position-only archetype
        world.remove_component::<Velocity>(entity1).unwrap();

        // entity2 should still have correct data (it's now alone in Position+Velocity archetype)
        let pos2 = world.get_component::<Position>(entity2).unwrap();
        let vel2 = world.get_component::<Velocity>(entity2).unwrap();
        assert_eq!(pos2.x, 2.0, "entity2 Position.x should still be 2.0");
        assert_eq!(vel2.x, 102.0, "entity2 Velocity.x should still be 102.0");

        // entity1 should only have Position now
        assert!(world.get_component::<Position>(entity1).is_some());
        assert!(world.get_component::<Velocity>(entity1).is_none());
        assert_eq!(world.get_component::<Position>(entity1).unwrap().x, 1.0);

        println!("✓ Component cleanup on archetype migration works correctly!");
    }

    /// Tests that `IteratorTimings` detects duplicate labels within a frame.
    ///
    /// Two iterators with the same label will corrupt the per-label splitting hint.
    /// This test simulates the logic inside `ParQueryIter::for_each`.
    #[test]
    fn test_per_label_duplicate_detection() {
        let timing = std::sync::Mutex::new(IteratorTimings::new());

        // Simulate iterator-1 with label "physics".
        {
            let mut t = timing.lock().unwrap();
            assert!(!t.visited_iterator_labels.contains(&"physics"));
            t.visited_iterator_labels.push("physics");
            t.per_iterator_label_average_duration
                .insert("physics", 120_000);
        }

        // Simulate iterator-2 with label "ai" - different label, no duplicate.
        {
            let mut t = timing.lock().unwrap();
            assert!(!t.visited_iterator_labels.contains(&"ai"));
            t.visited_iterator_labels.push("ai");
            t.per_iterator_label_average_duration.insert("ai", 50_000);
        }

        // Simulate a second "physics" iterator - same label, DUPLICATE.
        {
            let mut t = timing.lock().unwrap();
            assert!(t.visited_iterator_labels.contains(&"physics"));
            t.visited_duplicated_iterator_labels.push("physics");
            // Overwrites the splitting hint - exactly the problem we're detecting.
            t.per_iterator_label_average_duration
                .insert("physics", 800_000);
        }

        let t = timing.lock().unwrap();
        assert_eq!(t.visited_duplicated_iterator_labels, vec!["physics"]);
        assert_eq!(t.visited_iterator_labels, vec!["physics", "ai"]);
        // "physics" splitting hint was corrupted by the second write.
        assert_eq!(t.per_iterator_label_average_duration["physics"], 800_000);

        println!("✓ Duplicate label detection works correctly!");
    }

    /// The editor's Hierarchy source: `entity_rows` lists every live entity
    /// with its component names, sorted by entity id.
    #[test]
    fn entity_rows_list_live_entities_with_components() {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();

        let a = world
            .create_entity()
            .with(Position { x: 1.0, y: 2.0 })
            .with(Velocity { x: 0.0, y: 0.0 })
            .build()
            .unwrap();
        let _b = world
            .create_entity()
            .with(Position { x: 3.0, y: 4.0 })
            .build()
            .unwrap();

        let rows = world.entity_rows();
        assert_eq!(rows.len(), 2);
        // Deterministic ordering by entity id: `a` was created first (id 0).
        assert_eq!(rows[0].entity, a);
        assert_eq!(rows[0].components.len(), 2);
        assert!(rows[0]
            .components
            .iter()
            .any(|name| name.contains("Position")));
        assert!(rows[0]
            .components
            .iter()
            .any(|name| name.contains("Velocity")));
        assert_eq!(rows[1].components.len(), 1);

        // A destroyed entity no longer appears and reports no names.
        let _ = world.destroy_entity(a);
        assert_eq!(world.entity_rows().len(), 1);
        assert_eq!(world.entity_component_names(a), None);
    }

    /// `entity_component_names` includes runtime-defined (descriptor)
    /// components, and `resolve_entity_component_id` is scoped to the entity's
    /// archetype.
    #[test]
    fn descriptor_components_appear_and_resolution_is_archetype_scoped() {
        let mut world = World::new();
        world.register_component::<Position>();
        let descriptor = world
            .register_component_descriptor(
                0xABCD,
                "Demo.Thing",
                4,
                4,
                99,
                Blittability::engine_verified(),
            )
            .unwrap();

        let with_descriptor = world
            .create_descriptor_entity(&[(descriptor, 7_u32.to_ne_bytes().to_vec())])
            .unwrap();
        let with_position = world
            .create_entity()
            .with(Position { x: 0.0, y: 0.0 })
            .build()
            .unwrap();

        let names = world
            .entity_component_names(with_descriptor)
            .expect("entity alive");
        assert_eq!(names, vec!["Demo.Thing"]);

        // Resolution is per-entity: the descriptor component only resolves on
        // the entity that carries it, and Position only on its own entity.
        assert_eq!(
            world.resolve_entity_component_id(with_descriptor, "Demo.Thing"),
            Some(descriptor)
        );
        assert_eq!(
            world.resolve_entity_component_id(with_position, "Demo.Thing"),
            None
        );
        assert_eq!(
            world.resolve_entity_component_id(with_position, &type_name_of::<Position>()),
            Some(ComponentId::of::<Position>())
        );
    }

    /// `registered_components` lists every type (native and descriptor), sorted.
    #[test]
    fn registered_components_lists_every_type_sorted() {
        let mut world = World::new();
        world.register_component::<Velocity>();
        world.register_component::<Position>();
        world
            .register_component_descriptor(
                0x1111,
                "Demo.Alpha",
                4,
                4,
                1,
                Blittability::engine_verified(),
            )
            .unwrap();

        let registered = world.registered_components();
        assert_eq!(registered.len(), 3);
        // Sorted by name (full type paths sort before the demo name here).
        let names: Vec<String> = registered.iter().map(|(name, _)| name.clone()).collect();
        assert_eq!(names[0], "Demo.Alpha");
        assert!(names[1].contains("Position"));
        assert!(names[2].contains("Velocity"));
    }

    /// A tiny helper to get a component's registered type name without
    /// depending on the registry ordering in this test module.
    fn type_name_of<T: 'static>() -> String {
        std::any::type_name::<T>().to_string()
    }

    // -------------------------------------------------------------------------
    // Resource re-homing
    // -------------------------------------------------------------------------

    /// Counts its own drops, so a re-home that corrupted the stored function
    /// table shows up as a missing or doubled drop rather than as silence.
    ///
    /// The counter is owned per instance rather than being a `static`: the
    /// harness runs these tests in parallel, and a shared counter makes every
    /// `before + 1` assertion race with the others.
    #[derive(Debug)]
    struct RehomeProbe(std::sync::Arc<std::sync::atomic::AtomicUsize>);
    impl crate::resource::Resource for RehomeProbe {}

    impl Drop for RehomeProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// A fresh probe and the counter watching it.
    fn rehome_probe() -> (RehomeProbe, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let drops = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        (RehomeProbe(std::sync::Arc::clone(&drops)), drops)
    }

    /// Reads one probe's counter.
    fn drops_of(counter: &std::sync::atomic::AtomicUsize) -> usize {
        counter.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Inserting a resource records its function table, so a later reload has
    /// something to re-home from even if it never inserts the value again.
    #[test]
    fn inserting_a_resource_records_its_function_table() {
        let mut world = World::new();
        let id = crate::resource::ResourceId::of::<RehomeProbe>();

        assert!(!world.resource_factories.contains_key(&id));
        world.insert_resource(rehome_probe().0);
        assert!(world.resource_factories.contains_key(&id));
    }

    /// `register_resource` records the table without inserting a value, which
    /// is how a reloaded generation keeps an existing resource re-homeable.
    #[test]
    fn registering_a_resource_records_the_table_without_a_value() {
        let mut world = World::new();
        let id = crate::resource::ResourceId::of::<RehomeProbe>();

        world.register_resource::<RehomeProbe>();

        assert!(world.resource_factories.contains_key(&id));
        assert!(
            !world.has_resource::<RehomeProbe>(),
            "no value was inserted"
        );
    }

    /// Re-homing leaves the value intact and still droppable exactly once.
    #[test]
    fn rehoming_resources_preserves_the_value_and_its_drop() {
        let (probe, drops) = rehome_probe();
        let mut world = World::new();
        world.insert_resource(probe);

        // Stands in for a reloaded generation re-registering the type.
        world.register_resource::<RehomeProbe>();
        world.rehome_resources();

        assert!(world.has_resource::<RehomeProbe>(), "the value survives");
        assert_eq!(drops_of(&drops), 0, "nothing was dropped");

        drop(world);
        assert_eq!(
            drops_of(&drops),
            1,
            "the value is dropped exactly once after a re-home"
        );
    }

    /// The registration log reports what one `init` claimed, which is what
    /// distinguishes a retired owner from a live one.
    ///
    /// `resource_factories` cannot answer that: it accumulates and is never
    /// pruned, so a retired module's entry looks identical to a live one.
    #[test]
    fn the_registration_log_reports_what_one_generation_claimed() {
        let mut world = World::new();
        world.insert_resource(rehome_probe().0);

        // Stands in for the moment just before a module's `init` runs.
        let before_init = world.resource_registration_sequence();
        world.register_resource::<artifact_a::Settings>();

        assert_eq!(
            world.resource_ids_registered_since(before_init),
            vec![crate::resource::ResourceId::of::<artifact_a::Settings>()],
            "only what this generation registered, not everything ever registered"
        );
    }

    /// Dropping a retired owner's resource releases its value, which is what
    /// the host must do while the owning image is still mapped.
    #[test]
    fn dropping_a_retired_owners_resource_releases_its_value() {
        let (probe, drops) = rehome_probe();
        let mut world = World::new();
        world.insert_resource(probe);
        let id = crate::resource::ResourceId::of::<RehomeProbe>();

        assert_eq!(world.drop_resources(&[id]), 1);
        assert_eq!(drops_of(&drops), 1, "the value was dropped, not leaked");
        assert!(!world.has_resource::<RehomeProbe>());
        // Its bookkeeping goes too, so a later insert starts clean.
        assert!(!world.resource_factories.contains_key(&id));
    }

    /// Dropping an id the world does not hold is harmless and reports zero.
    #[test]
    fn dropping_an_absent_resource_is_a_no_op() {
        let mut world = World::new();
        let id = crate::resource::ResourceId::of::<RehomeProbe>();
        assert_eq!(world.drop_resources(&[id]), 0);
    }

    /// Removing a resource forgets its table, so a later resource registered
    /// under the same id cannot inherit a stale one.
    #[test]
    fn removing_a_resource_forgets_its_function_table() {
        let mut world = World::new();
        let id = crate::resource::ResourceId::of::<RehomeProbe>();
        world.insert_resource(rehome_probe().0);

        world
            .remove_resource::<RehomeProbe>()
            .expect("it was there");

        assert!(!world.resource_factories.contains_key(&id));
    }

    // -------------------------------------------------------------------------
    // Shared resource identity
    // -------------------------------------------------------------------------

    // A shared resource type, standing for one artifact's copy. The
    // cross-artifact behaviour itself lives in
    // `tests/shared_resource_identity.rs`; what stays here needs access to the
    // world's private bookkeeping, which an integration test cannot reach.
    mod artifact_a {
        /// `demo::Settings` as one artifact compiled it.
        #[derive(Debug)]
        pub struct Settings {
            // The shape is what the identity checks carry; nothing reads the
            // value.
            #[allow(dead_code)]
            pub value: u32,
        }
        impl crate::resource::Resource for Settings {
            fn shared_name() -> Option<&'static str> {
                Some("demo::Settings")
            }
        }
    }

    /// An ordinary resource is untouched: its id is its `TypeId`, and the box
    /// still checks that exactly.
    #[test]
    fn an_ordinary_resource_keeps_the_strict_identity_check() {
        let mut world = World::new();
        world.insert_resource(rehome_probe().0);

        assert!(!world
            .resources
            .get(&crate::resource::ResourceId::of::<RehomeProbe>())
            .unwrap()
            .has_shared_identity());
        assert!(world.take_registration_error().is_none());
    }

    /// A released bit cannot alias two component sets: the archetype that
    /// carried it is gone before the bit is reusable, so the next type to take
    /// it gets its own column and its own archetype.
    #[test]
    fn recycled_bit_does_not_alias_archetypes() {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();
        let velocity_id = ComponentId::of::<Velocity>();
        let released_bit = world
            .component_registry()
            .get_bit(&velocity_id)
            .expect("registered");
        let kept = world
            .create_entity()
            .with(Position { x: 1.0, y: 2.0 })
            .with(Velocity { x: 0.0, y: 0.0 })
            .build()
            .unwrap();

        let dropped =
            world.drop_forgotten_components(&[std::any::type_name::<Velocity>().to_string()]);
        assert_eq!(dropped, 1, "the entity's velocity row was rehomed out");
        assert_eq!(
            world.component_registry().get_bit(&velocity_id),
            None,
            "the registration is gone with the rows"
        );

        // The next registration takes the freed bit...
        world.register_component::<Health>();
        let health_id = ComponentId::of::<Health>();
        assert_eq!(
            world.component_registry().get_bit(&health_id),
            Some(released_bit),
            "the freed bit is the one reused"
        );

        // ...and the archetype it creates is its own, holding exactly its row.
        let health_entity = world
            .create_entity()
            .with(Position { x: 3.0, y: 4.0 })
            .with(Health { hp: 5 })
            .build()
            .unwrap();
        assert_eq!(world.live_row_count(health_id), 1);
        assert_eq!(world.live_row_count(ComponentId::of::<Position>()), 2);
        assert_eq!(
            world.archetypes.len(),
            2,
            "position-only and position+health are two archetypes"
        );
        let location = world.entity_locations[&health_entity];
        let archetype = &world.archetypes[&location.archetype_id];
        assert!(archetype.component_types.contains(&health_id));
        assert!(!archetype.component_types.contains(&velocity_id));
        assert!(world.entity_locations.contains_key(&kept));
    }

    /// A re-registration updates the id's stamp instead of adding an entry:
    /// the window stays proportional to distinct resources, and a generation
    /// that re-registers its resources is exactly what the host's reload diff
    /// has to see.
    #[test]
    fn a_re_registered_resource_stays_one_registration() {
        let mut world = World::new();
        world.insert_resource(AccountedResource { value: 1 });
        let sequence = world.resource_registration_sequence();

        world.insert_resource(AccountedResource { value: 2 });
        world.insert_resource(AccountedResource { value: 3 });
        assert_eq!(
            world.get_resource::<AccountedResource>().unwrap().value,
            3,
            "the replacement really replaced the value"
        );
        let id = crate::resource::ResourceId::of::<AccountedResource>();
        assert_eq!(
            world.resource_ids_registered_since(sequence),
            vec![id],
            "a re-registration inside the window is reported, exactly once"
        );

        world.insert_resource(FreshAccountedResource { value: 4 });
        assert_eq!(
            world
                .get_resource::<FreshAccountedResource>()
                .unwrap()
                .value,
            4
        );
        let mut claimed = world.resource_ids_registered_since(sequence);
        claimed.sort_unstable();
        let mut expected = vec![
            id,
            crate::resource::ResourceId::of::<FreshAccountedResource>(),
        ];
        expected.sort_unstable();
        assert_eq!(
            claimed, expected,
            "the fresh id joins the window; nothing accumulates per insert"
        );
        assert_eq!(
            world.resource_registration_stamps.len(),
            2,
            "one entry per distinct id, however many times it was written"
        );
    }

    /// A resource another subject still claims is not dropped by one subject's
    /// retirement: the claim refcount keeps the value alive until the last
    /// claimant lets go.
    #[test]
    fn a_shared_claim_survives_one_subjects_retirement() {
        let mut world = World::new();
        let id = crate::resource::ResourceId::of::<AccountedResource>();
        world.insert_resource(AccountedResource { value: 1 });

        // Two subjects registered it.
        world.retain_resource_claims(&[id]);
        world.retain_resource_claims(&[id]);

        // One retires it: the value stays, claim and all.
        world.release_resource_claims(&[id]);
        assert_eq!(
            world.drop_resources(&[id]),
            0,
            "a live claim keeps the value standing"
        );
        assert!(world.get_resource::<AccountedResource>().is_some());

        // The last claimant retires it too.
        world.release_resource_claims(&[id]);
        assert_eq!(world.drop_resources(&[id]), 1, "the last release frees it");
        assert!(world.get_resource::<AccountedResource>().is_none());
    }

    /// A relayout republishes the whole registry layout - alignment and schema
    /// hash included - so `get_layout` never describes a column that is gone.
    #[test]
    fn relayout_republishes_the_registry_layout() {
        let mut world = World::new();
        let component_id = world
            .register_component_descriptor(
                0xD9,
                "Project.Republished",
                8,
                4,
                100,
                Blittability::engine_verified(),
            )
            .unwrap();
        world
            .create_descriptor_entity(&[(component_id, vec![0; 8])])
            .unwrap();

        world
            .relayout_descriptor_component(component_id, 16, 8, 200, &FieldPlan::new())
            .expect("the empty plan fits both layouts");

        let record = world
            .component_registry()
            .get_layout(&component_id)
            .expect("registered");
        assert_eq!(record.size, 16);
        assert_eq!(
            record.align, 8,
            "the registration-time placeholder alignment moved with the storage"
        );
        assert_eq!(record.schema_hash, Some(200));
        let Some(StorageFactory::Descriptor(layout)) = world.storage_factories.get(&component_id)
        else {
            panic!("the factory is not a descriptor factory");
        };
        assert_eq!(
            (layout.size, layout.align, layout.schema_hash),
            (record.size, record.align, record.schema_hash.unwrap())
        );
    }

    // -------------------------------------------------------------------------
    // Foreign resources
    // -------------------------------------------------------------------------

    /// A declared name hashes the engine's way, so a declaration from another
    /// language and a Rust type that writes down the same string are one
    /// resource - which is the whole point of the name being declared rather
    /// than derived.
    #[test]
    fn a_foreign_declaration_hashes_to_the_rust_types_id() {
        let mut world = World::new();
        let id = world
            .register_foreign_resource("demo::Settings", "Project.Settings", 4, 4, 7)
            .expect("a fresh name is claimed");

        assert_eq!(
            id,
            crate::resource::ResourceId::of::<artifact_a::Settings>()
        );
        assert_eq!(
            id,
            crate::resource::ResourceId::Shared(crate::component::shared_component_identity(
                "demo::Settings"
            ))
        );
        assert_eq!(world.foreign_resource_layout(id), Some((4, 4, 7)));
        assert_eq!(
            world.shared_resource_names(),
            vec!["demo::Settings".to_string()]
        );
    }

    /// The claim guard keeps its Rust-versus-Rust name check and takes the
    /// layout as the cross-language one: a matching declaration joins, a
    /// mismatched one is refused.
    #[test]
    fn a_foreign_declaration_joins_by_layout_and_is_refused_by_a_mismatch() {
        let mut world = World::new();
        world.register_resource::<artifact_a::Settings>();
        assert!(world.take_registration_error().is_none());

        let id = world
            .register_foreign_resource("demo::Settings", "Project.Settings", 4, 4, 7)
            .expect("the same layout joins");
        assert_eq!(
            id,
            crate::resource::ResourceId::of::<artifact_a::Settings>()
        );
        assert!(
            world.take_registration_error().is_none(),
            "a matching declaration is not a conflict"
        );

        let error = world
            .register_foreign_resource("demo::Settings", "Project.Settings", 16, 8, 7)
            .expect_err("a different layout cannot be joined");
        assert!(matches!(
            error,
            WorldError::SharedResourceLayoutMismatch { .. }
        ));
        assert!(
            world.take_registration_error().is_none(),
            "the refusal goes back to the caller that made it, not to the next init drain"
        );
    }

    /// Bytes round-trip through a declaration, the size is checked, and a
    /// mutable view stamps the change tick as it is handed out.
    #[test]
    fn foreign_bytes_round_trip_and_stamp_the_change_tick() {
        let mut world = World::new();
        let id = world
            .register_foreign_resource("demo::Bytes", "Project.Bytes", 8, 4, 7)
            .expect("a fresh name is claimed");
        assert!(world.foreign_resource_bytes(id).is_none(), "no value yet");

        world
            .insert_foreign_resource_bytes(id, &[1, 2, 3, 4, 5, 6, 7, 8])
            .expect("the payload matches the size");
        assert_eq!(
            world.foreign_resource_bytes(id),
            Some([1_u8, 2, 3, 4, 5, 6, 7, 8].as_slice())
        );

        assert!(matches!(
            world.insert_foreign_resource_bytes(id, &[0; 4]),
            Err(WorldError::ForeignResourceBytesMismatch { .. })
        ));
        assert_eq!(
            world.foreign_resource_bytes(id).unwrap().len(),
            8,
            "a refused payload leaves the stored value alone"
        );

        world.increment_change_tick();
        let expected_tick = world.change_tick();
        let (bytes, ticks) = world
            .foreign_resource_bytes_mut(id)
            .expect("the value is there");
        bytes[0] = 9;
        assert_eq!(ticks.changed, expected_tick);
        assert_eq!(world.foreign_resource_bytes(id).unwrap()[0], 9);
    }

    /// A private resource has no byte view: its id is a `TypeId`, which no
    /// other language can name, so there is nothing for one to address.
    #[test]
    fn a_private_resource_has_no_foreign_view() {
        let mut world = World::new();
        world.insert_resource(rehome_probe().0);
        let id = crate::resource::ResourceId::of::<RehomeProbe>();

        assert!(world.foreign_resource_bytes(id).is_none());
        assert!(world.foreign_resource_bytes_mut(id).is_none());
        assert!(matches!(
            world.insert_foreign_resource_bytes(id, &[0; 8]),
            Err(WorldError::SharedResourceNotRegistered { .. })
        ));
        assert!(matches!(
            world.relayout_foreign_resource(id, 8, 4, 1, &FieldPlan::new()),
            Err(WorldError::ForeignResourceNotRegistered { .. })
        ));
    }

    /// A relayout moves the stored bytes through the plan, updates the declared
    /// layout, and reports how many values it migrated.
    #[test]
    fn relayout_migrates_a_foreign_value_through_the_plan() {
        let mut world = World::new();
        let id = world
            .register_foreign_resource("demo::Relayout", "Project.Relayout", 8, 4, 1)
            .expect("a fresh name is claimed");

        // `b`, then `a`, then an eight-byte field that did not exist.
        let plan = FieldPlan::between(
            &[
                LayoutField {
                    name: "a",
                    type_tag: "u32",
                    offset: 0,
                    size: 4,
                },
                LayoutField {
                    name: "b",
                    type_tag: "u32",
                    offset: 4,
                    size: 4,
                },
            ],
            &[
                LayoutField {
                    name: "b",
                    type_tag: "u32",
                    offset: 0,
                    size: 4,
                },
                LayoutField {
                    name: "a",
                    type_tag: "u32",
                    offset: 4,
                    size: 4,
                },
                LayoutField {
                    name: "added",
                    type_tag: "u32",
                    offset: 8,
                    size: 8,
                },
            ],
        );

        // With no value stored, the declaration moves and nothing is migrated.
        assert_eq!(
            world
                .relayout_foreign_resource(id, 16, 8, 2, &plan)
                .expect("the declaration moves"),
            0
        );
        assert_eq!(world.foreign_resource_layout(id), Some((16, 8, 2)));

        world
            .insert_foreign_resource_bytes(id, &[1, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
            .expect("the payload matches the new size");
        assert_eq!(
            world
                .relayout_foreign_resource(id, 16, 8, 2, &FieldPlan::new())
                .expect("a value is there to migrate"),
            1
        );
        assert_eq!(
            world.foreign_resource_bytes(id),
            Some([0_u8; 16].as_slice()),
            "an empty plan zeroes the value it migrates"
        );
    }

    /// A Rust value's shape is its type, so a foreign declaration cannot
    /// reshape it, and a plan that does not fit the payload is refused before
    /// anything moves.
    #[test]
    fn relayout_refuses_a_rust_value_and_a_plan_that_leaves_the_payload() {
        let mut world = World::new();
        world.register_resource::<artifact_a::Settings>();
        let id = world
            .register_foreign_resource("demo::Settings", "Project.Settings", 4, 4, 1)
            .expect("the same layout joins");

        // A Rust value stored under the id brings its own table with it, so the
        // declaration is the Rust type's from then on and a foreign relayout is
        // no longer even addressed to one.
        world.insert_resource(artifact_a::Settings { value: 3 });
        assert!(matches!(
            world.relayout_foreign_resource(id, 8, 4, 2, &FieldPlan::new()),
            Err(WorldError::ForeignResourceNotRegistered { .. })
        ));

        // Declaring it foreign again - what the managed side does on every
        // reload - is refused: the stored value's destructor is the price of
        // admitting the declaration, so it is not admitted.
        assert!(matches!(
            world.register_foreign_resource("demo::Settings", "Project.Settings", 4, 4, 1),
            Err(WorldError::ForeignResourceHoldsRustValue { .. })
        ));
        assert_eq!(
            world.foreign_resource_layout(id),
            None,
            "the Rust value's table still owns the id"
        );

        // A declaration that was foreign from the start keeps its payload
        // checks: there, only the plan is in the way of a resize.
        let fresh = world
            .register_foreign_resource("demo::Other", "Project.Other", 4, 4, 1)
            .expect("a fresh name is claimed");
        world
            .insert_foreign_resource_bytes(fresh, &3_u32.to_ne_bytes())
            .expect("the payload matches the size");
        let mut too_wide = FieldPlan::new();
        too_wide.push(2, 4, FieldSource::OldOffset(0));
        assert!(matches!(
            world.relayout_foreign_resource(fresh, 4, 4, 2, &too_wide),
            Err(WorldError::ForeignResourcePlanOutOfBounds { .. })
        ));
        assert_eq!(
            world.foreign_resource_layout(fresh),
            Some((4, 4, 1)),
            "a refused relayout leaves the declaration as it was"
        );
    }

    /// Re-homing leaves a foreign box's table alone: those bytes are not the
    /// Rust type's to drop, even when it shares their name.
    #[test]
    fn rehoming_leaves_a_foreign_box_alone() {
        let mut world = World::new();
        let id = world
            .register_foreign_resource("demo::Settings", "Project.Settings", 4, 4, 1)
            .expect("a fresh name is claimed");
        world
            .insert_foreign_resource_bytes(id, &9_u32.to_ne_bytes())
            .expect("the payload matches the size");

        // The Rust owner registers the same name, which makes the declaration
        // Rust's while the stored bytes stay foreign - the split a re-home has
        // to respect. Without the skip it would hand these bytes the Rust
        // type's destructor, freeing memory that type never allocated.
        world.register_resource::<artifact_a::Settings>();
        assert!(world.resources[&id].is_foreign());
        assert_eq!(
            world.resource_factories.get(&id).map(|ops| ops.foreign),
            Some(false),
            "the Rust registration owns the declaration again"
        );

        world.rehome_resources();

        assert!(
            world.resources[&id].is_foreign(),
            "a foreign box keeps its own drop, whatever the factories say"
        );
        assert_eq!(
            world.resources[&id].bytes(),
            9_u32.to_ne_bytes().as_slice(),
            "leaving the table alone leaves the payload alone too"
        );
    }

    /// A foreign declaration cannot take over an id that stores a Rust value:
    /// the value's destructor is bound to its type, so its bytes are never
    /// reinterpreted - and the table that would have dropped nothing stays out.
    #[test]
    fn foreign_registration_refuses_a_stored_rust_value() {
        let mut world = World::new();
        world.register_resource::<artifact_a::Settings>();
        world.insert_resource(artifact_a::Settings { value: 7 });
        let id = crate::resource::ResourceId::of::<artifact_a::Settings>();

        assert!(matches!(
            world.register_foreign_resource("demo::Settings", "Project.Settings", 4, 4, 1),
            Err(WorldError::ForeignResourceHoldsRustValue { .. })
        ));
        assert_eq!(
            world.resource_factories.get(&id).map(|ops| ops.foreign),
            Some(false),
            "the Rust registration still owns the id"
        );
        assert_eq!(
            world
                .get_resource::<artifact_a::Settings>()
                .map(|v| v.value),
            Some(7),
            "the refusal left the value alone"
        );
    }

    /// Byte views are for foreign payloads only: a Rust value that declares a
    /// shared name is not served as bytes, and the refusal costs nothing - no
    /// tick is stamped and no payload is displaced.
    #[test]
    fn foreign_byte_views_refuse_a_rust_owned_resource() {
        let mut world = World::new();
        world.register_resource::<artifact_a::Settings>();
        world.insert_resource(artifact_a::Settings { value: 5 });
        let id = crate::resource::ResourceId::of::<artifact_a::Settings>();

        assert!(world.foreign_resource_bytes(id).is_none());
        assert!(world.foreign_resource_bytes_mut(id).is_none());
        assert!(matches!(
            world.insert_foreign_resource_bytes(id, &5_u32.to_ne_bytes()),
            Err(WorldError::ForeignResourceFactoryIsNative { .. })
        ));
        assert_eq!(
            world
                .get_resource::<artifact_a::Settings>()
                .map(|v| v.value),
            Some(5),
            "the value survived the refusals"
        );
    }

    /// A zero-sized Rust type is a component like any other.
    ///
    /// This is the tag/marker idiom (`struct Enemy;`), and the unification broke it:
    /// the native registration path fed the descriptor lane's zero-width refusal
    /// into an `.expect`, so declaring a marker aborted the process. No test
    /// covered the case, which is why every gate stayed green through the change.
    #[test]
    fn a_zero_sized_component_lives_a_full_life() {
        #[derive(Clone, Debug, Default)]
        struct Enemy;
        impl Component for Enemy {}

        #[derive(Clone, Debug, Default)]
        struct Health {
            points: u32,
        }
        impl Component for Health {}

        let mut world = World::new();
        world.register_component::<Enemy>();
        world.register_component::<Health>();

        // Spawn: a marker alone, and a marker beside a sized component, so the
        // zero-sized column is exercised both as an archetype's only column and as
        // one of several.
        let lone = world.create_entity().with(Enemy).build().unwrap();
        let paired = world
            .create_entity()
            .with(Enemy)
            .with(Health { points: 7 })
            .build()
            .unwrap();

        assert!(world.get_component::<Enemy>(lone).is_some());
        assert!(world.get_component::<Enemy>(paired).is_some());

        // Column contents: a marker is a filter, which is the whole reason to
        // declare one, so both rows have to be in the column the filter reads.
        let marker = ComponentId::of::<Enemy>();
        let rows: usize = world
            .archetypes
            .values()
            .filter_map(|archetype| archetype.component_storages.get(marker))
            .map(|column| column.len())
            .sum();
        assert_eq!(
            rows, 2,
            "both markers occupy a row in the zero-sized column"
        );

        // Archetype move: adding a component migrates every column, the zero-sized
        // one included, and its rows have no bytes to carry.
        world
            .add_component(lone, Health { points: 3 })
            .expect("the marker's entity accepts another component");
        assert!(world.get_component::<Enemy>(lone).is_some());
        assert_eq!(world.get_component::<Health>(lone).unwrap().points, 3);

        // Removal: the swap-remove that closes the gap copies zero bytes, and the
        // surviving row must still be found.
        world
            .remove_component::<Enemy>(paired)
            .expect("the marker comes off");
        assert!(world.get_component::<Enemy>(paired).is_none());
        assert!(
            world.get_component::<Enemy>(lone).is_some(),
            "removing one marker leaves the other"
        );
        assert_eq!(
            world.get_component::<Health>(paired).unwrap().points,
            7,
            "the sized companion is untouched by the marker's removal"
        );

        // Destruction: the column frees a buffer it never allocated.
        assert!(world.destroy_entity(lone), "the entity is destroyed");
        assert!(world.get_component::<Enemy>(lone).is_none());
    }

    /// A zero-sized component with a wider alignment is still addressable.
    ///
    /// A column that never allocates keeps its dangling pointer for life, so that
    /// pointer has to be aligned for the element rather than for `u8`. Reading a
    /// row through a misaligned pointer is undefined behaviour even when the row
    /// has no bytes, so this pins the alignment rather than the size.
    #[test]
    fn an_over_aligned_zero_sized_component_is_addressable() {
        #[derive(Clone, Debug, Default)]
        #[repr(align(16))]
        struct AlignedTag;
        impl Component for AlignedTag {}

        assert_eq!(std::mem::size_of::<AlignedTag>(), 0);
        assert_eq!(std::mem::align_of::<AlignedTag>(), 16);

        let mut world = World::new();
        world.register_component::<AlignedTag>();
        let entity = world.create_entity().with(AlignedTag).build().unwrap();

        assert!(world.get_component::<AlignedTag>(entity).is_some());
    }

    /// A zero-width *descriptor* declaration stays refused.
    ///
    /// The verdict is split, not loosened: a descriptor's width arrives from a
    /// manifest another language wrote, where zero means that declaration is wrong.
    /// Pinned so a later change cannot quietly collapse the two rules back into
    /// one and start accepting a malformed manifest.
    #[test]
    fn a_zero_width_descriptor_component_is_still_refused() {
        let mut world = World::new();

        let refused = world.register_component_descriptor(
            0x2E_0001,
            "probe::ZeroWidth".to_string(),
            0,
            4,
            11,
            Blittability::from_manifest_fields(),
        );

        assert!(
            matches!(refused, Err(WorldError::DescriptorSizeZero)),
            "a zero-width manifest declaration is a declaration error, not a marker"
        );
    }

    /// A foreign resource's declared fields are stored and served back.
    ///
    /// Without this a foreign resource is opaque bytes: the editor has nothing
    /// to name, and neither does anything else that wants more than a width.
    #[test]
    fn a_foreign_resource_serves_its_declared_field_layout() {
        let mut world = World::new();
        let id = world
            .register_foreign_resource("probe::Tuning", "Probe.Tuning", 8, 4, 1)
            .expect("a valid foreign layout registers");

        assert!(
            world.resource_field_layout(id).is_none(),
            "a resource registered without a layout is simply not inspectable"
        );

        world
            .register_resource_field_layout(
                id,
                vec![
                    crate::component_registry::ComponentFieldDescriptor {
                        name: "speed",
                        type_tag: "f32",
                        offset: 0,
                        size: 4,
                        align: 4,
                        element_count: 0,
                    },
                    crate::component_registry::ComponentFieldDescriptor {
                        name: "gain",
                        type_tag: "f32",
                        offset: 4,
                        size: 4,
                        align: 4,
                        element_count: 0,
                    },
                ],
            )
            .expect("the fields fit the registered size");

        let fields = world
            .resource_field_layout(id)
            .expect("the layout is stored");
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].name, "speed");
        assert_eq!(fields[1].offset, 4);
    }

    /// A field layout that cannot describe the resource is refused.
    ///
    /// Both checks the descriptor-component path applies, for the same reason:
    /// a foreign resource's bytes are as opaque as a descriptor row, so a
    /// reader has to be kept inside the value and away from any pointer pair
    /// the bytes never held.
    #[test]
    fn an_impossible_resource_field_layout_is_refused() {
        let mut world = World::new();
        let id = world
            .register_foreign_resource("probe::Narrow", "Probe.Narrow", 4, 4, 1)
            .expect("a valid foreign layout registers");

        let past_the_end = world.register_resource_field_layout(
            id,
            vec![crate::component_registry::ComponentFieldDescriptor {
                name: "wide",
                type_tag: "u64",
                offset: 0,
                size: 8,
                align: 8,
                element_count: 0,
            }],
        );
        assert!(
            past_the_end.is_err(),
            "a field may not reach past the value"
        );

        let container = world.register_resource_field_layout(
            id,
            vec![crate::component_registry::ComponentFieldDescriptor {
                name: "items",
                type_tag: "vec:f32",
                offset: 0,
                size: 4,
                align: 4,
                element_count: 0,
            }],
        );
        assert!(
            container.is_err(),
            "a container tag would be read as a pointer and length the bytes do not hold"
        );
    }

    /// A relayout drops the layout it described.
    ///
    /// Serving the previous generation's offsets over migrated bytes would be
    /// worse than serving nothing, so the engine forgets and the declarer
    /// re-publishes - which is what the host does on the same pass.
    #[test]
    fn a_relayout_drops_the_stored_resource_field_layout() {
        let mut world = World::new();
        let id = world
            .register_foreign_resource("probe::Moving", "Probe.Moving", 4, 4, 1)
            .expect("a valid foreign layout registers");
        world
            .register_resource_field_layout(
                id,
                vec![crate::component_registry::ComponentFieldDescriptor {
                    name: "value",
                    type_tag: "u32",
                    offset: 0,
                    size: 4,
                    align: 4,
                    element_count: 0,
                }],
            )
            .expect("the field fits");
        assert!(world.resource_field_layout(id).is_some());

        let plan = FieldPlan::between(
            &[LayoutField {
                name: "value",
                type_tag: "u32",
                offset: 0,
                size: 4,
            }],
            &[
                LayoutField {
                    name: "value",
                    type_tag: "u32",
                    offset: 0,
                    size: 4,
                },
                LayoutField {
                    name: "added",
                    type_tag: "u32",
                    offset: 4,
                    size: 4,
                },
            ],
        );
        world
            .relayout_foreign_resource(id, 8, 4, 2, &plan)
            .expect("the wider shape is valid");

        assert!(
            world.resource_field_layout(id).is_none(),
            "the layout that described the old shape is forgotten"
        );
    }
}
