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
use std::any::Any;
use std::collections::HashMap;

// External crates
use pill_core::{error, warn};
use trait_type_map::{ErasedVecStorageInfo, TraitAccessible, TraitTypeMap, VecFamily};

// Current crate
use crate::archetype::{Archetype, ArchetypeId, DynamicComponentLayout, StorageFactory};
use crate::commands::CommandQueue;
use crate::component::{
    Component, ComponentId, ComponentMask, ComponentRegistry, ComponentTicks, Tick,
};
use crate::entity::Entity;
use crate::query::change_detection::Mut;
use crate::resource::{Resource, ResourceId};
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
// In sequential mode the override stays None, so queries fall back to the
// shared world.system_last_run field - no thread-local overhead.

thread_local! {
    /// Each thread's private "my system last ran at tick ___" value.
    /// `None` means "not in a parallel batch - use the world field."
    static PER_THREAD_LAST_RUN_TICK: std::cell::Cell<Option<Tick>> =
        const { std::cell::Cell::new(None) };
}

#[inline]
fn per_thread_last_run_tick() -> Option<Tick> {
    PER_THREAD_LAST_RUN_TICK.with(|cell| cell.get())
}

/// Store a per-thread baseline tick, returning the old value so the
/// caller can restore it when the system finishes (RAII-style).
#[inline]
pub(crate) fn set_per_thread_last_run_tick(value: Option<Tick>) -> Option<Tick> {
    PER_THREAD_LAST_RUN_TICK.with(|cell| cell.replace(value))
}

// =============================================================================
// Component Copier Type
// =============================================================================

/// Function that copies a component from one storage to another at given indices.
type ComponentCopier = fn(
    source: &TraitTypeMap<dyn Component, VecFamily>,
    destination: &mut TraitTypeMap<dyn Component, VecFamily>,
    index: usize,
);

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
type ScriptUpdater =
    fn(&mut TraitTypeMap<dyn Component, VecFamily>, usize, Entity, *mut World, *mut CommandQueue);

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
/// Native components submit a `&'static` slice from their declaring
/// artifact's static data. Components defined by another language describe
/// themselves in a runtime manifest, so their layout is owned by the `World`
/// instead. The editor reads either through [`World::component_field_layout`],
/// which erases the distinction.
#[derive(Debug, Clone)]
pub(crate) enum ComponentFieldLayout {
    /// Compile-time layout living in the declaring artifact's static data.
    Static(&'static [crate::component_registry::ComponentFieldDescriptor]),
    /// Runtime-described layout owned by the world (foreign-language
    /// components).
    Owned(Vec<crate::component_registry::ComponentFieldDescriptor>),
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
    /// Component copiers for moving entities between archetypes
    pub(crate) component_copiers: HashMap<ComponentId, ComponentCopier>,
    /// Script component types (ComponentId, component mask bit)
    script_components: Vec<(ComponentId, u8)>,
    /// Script updaters for calling update() on script components
    script_updaters: HashMap<ComponentId, ScriptUpdater>,
    /// Component registry for bit indices and names
    pub(crate) component_registry: ComponentRegistry,
    /// Resources (singleton data) stored by type
    pub(crate) resources: HashMap<ResourceId, Box<dyn Any + Send + Sync>>,
    /// Per-resource change-detection ticks (parallel to `resources`).
    pub(crate) resource_ticks: HashMap<ResourceId, ComponentTicks>,
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
    #[cfg(debug_assertions)]
    pub(crate) debug_resource_write_locks: std::collections::HashSet<ResourceId>,

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
    /// Monotonic counter bumped on every component registration (plain or
    /// persistable), letting the host enumerate which types one module's
    /// `init` registered at all — the distinction between a type that was
    /// dropped entirely and one merely downgraded to a plain component.
    pub(crate) component_registration_sequence: u64,
    /// Chronological `(type_name, sequence)` log of every component
    /// registration, plain and persistable alike.
    pub(crate) component_registration_log: Vec<(String, u64)>,
    /// Field layouts submitted by `#[derive(PillComponent)]` (static, living
    /// in the declaring artifact) or described at runtime by a foreign-language
    /// manifest (owned). Consumed by the C# mirror codegen and the editor's
    /// generic inspector. Re-registered by each reloaded generation; a dynamic
    /// manifest replaces rather than accumulates.
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
            component_copiers: HashMap::new(),
            script_components: Vec::new(),
            script_updaters: HashMap::new(),
            component_registry: ComponentRegistry::new(),
            resources: HashMap::new(),
            resource_ticks: HashMap::new(),
            change_tick: 0,
            system_last_run: 0,
            archetype_generation: 0,
            #[cfg(debug_assertions)]
            debug_resource_write_locks: std::collections::HashSet::new(),
            commands_executed_this_frame: 0,
            iterator_timings: std::sync::Arc::new(std::sync::Mutex::new(IteratorTimings::new())),
            persist_serializers: HashMap::new(),
            persist_deserializers: HashMap::new(),
            persist_inserters: HashMap::new(),
            persist_schema_hashes: HashMap::new(),
            persist_registration_sequence: 0,
            persist_registration_log: Vec::new(),
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
        T: Component + TraitAccessible<dyn Component>,
    {
        let component_id = ComponentId::of::<T>();
        for archetype in self.archetypes.values_mut() {
            if archetype.component_types.contains(&component_id) {
                let storage = archetype.component_storages.get_storage_mut::<T>();
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
        T: Component + TraitAccessible<dyn Component>,
    {
        let component_id = ComponentId::of::<T>();
        let archetype = self
            .archetypes
            .values_mut()
            .filter(|archetype| archetype.component_types.contains(&component_id))
            .nth(chunk_index)?;
        let archetype_id = archetype.id;
        let storage = archetype.component_storages.get_storage_mut::<T>();
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
        T: Component + TraitAccessible<dyn Component>,
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
            .get_storage_mut::<T>()
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

    /// Return one archetype-sized entity chunk for language bindings.
    ///
    /// Entity chunks enumerate every archetype and provide the driver for
    /// `EntityTerm` and queries containing only optional component terms.
    pub fn entity_chunk(&self, chunk_index: usize) -> Option<(ArchetypeId, &[Entity])> {
        let archetype = self.archetypes.values().nth(chunk_index)?;
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
        T: Component + TraitAccessible<dyn Component> + Clone,
    {
        let _zone = crate::profile_scope!(
            "register component",
            [(
                "Component type being registered: {}",
                std::any::type_name::<T>()
            )]
        );
        let component_id = ComponentId::of::<T>();
        let type_name = std::any::type_name::<T>().to_string();

        // Shared renderer components (Position/Sprite/Color) are `repr(C)` ABI
        // types registered through this plain path, so a hand-written catalog
        // supplies their field layouts here - the editor then shows and edits
        // them like any `#[derive(PillComponent)]` type. Resolved up front
        // because `type_name` is moved into the registration log below.
        //
        // Unconditional: the catalog is pure data and now lives in the half of
        // `render` that carries no GPU dependency, so a headless world resolves
        // the same layouts a windowed one does. It used to be gated on the
        // `rendering` feature, which silently made the same component
        // inspectable in one build and not in the other.
        let shared_field_layout = crate::render::shared_component_field_layout(&type_name);

        // Register component (bit index + name)
        // `register_bit` rather than `register`: the world does not act on
        // whether the type was already present, and re-registration is normal
        // here because a hot reload re-runs every `init`.
        let bit = match self.component_registry.register_bit::<T>() {
            Ok(bit) => bit,
            Err(error) => {
                // The 128-type ceiling is a configuration outcome, not a
                // programming error, so it is reported as a first-class
                // diagnostic and recorded for the init entry point instead of
                // panicking. The caller (project/module init) fails the reload
                // transactionally when the error is drained by
                // `component_registry::register_all_components`.
                error!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    type_name = %type_name,
                    error = %error,
                    remaining = self.component_registry.available_slots(),
                    "component registration failed"
                );
                self.registration_error = Some(error);
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
        // told apart from one merely downgraded to a plain component.
        self.component_registration_log
            .push((type_name, self.component_registration_sequence));
        self.component_registration_sequence = self.component_registration_sequence.wrapping_add(1);

        // Register the storage factory as plain DATA (type id, layout, and a
        // per-type function table) instead of a closure that would be
        // monomorphized into this generation's DLL. The engine builds the
        // actual column in `Archetype::new` as a concrete `Box<ErasedVecStorage>`
        // with no trait-object vtable, and re-homes its function table on
        // every reload, so columns survive DLL unloads.
        self.storage_factories.insert(
            component_id,
            StorageFactory::Native(ErasedVecStorageInfo::<dyn Component>::of::<T>()),
        );

        // Register copier function for this component type.
        // Uses a named generic function (not a closure) so the fn pointer
        // requires no heap allocation or vtable dispatch.
        self.component_copiers
            .insert(component_id, copy_component::<T>);

        // Attach the shared renderer layout (if the catalog names this type),
        // giving hand-registered ABI components editable editor fields.
        if let Some(layout) = shared_field_layout {
            self.component_field_layouts
                .insert(component_id, ComponentFieldLayout::Static(layout));
        }
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

    /// Register a component together with its compile-time field layout, so
    /// the C# mirror codegen can emit a typed struct. Components registered
    /// without field metadata (hand-registered, dynamic, or unit types) keep
    /// the opaque ABI-blob mirror.
    pub fn register_component_with_layout<T>(
        &mut self,
        fields: &'static [crate::component_registry::ComponentFieldDescriptor],
    ) where
        T: Component + TraitAccessible<dyn Component> + Clone,
    {
        self.register_component::<T>();
        self.component_field_layouts
            .insert(ComponentId::of::<T>(), ComponentFieldLayout::Static(fields));
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
        match self.component_field_layouts.get(&component_id) {
            Some(ComponentFieldLayout::Static(fields)) => Some(fields),
            Some(ComponentFieldLayout::Owned(fields)) => Some(fields),
            None => None,
        }
    }

    /// Record a runtime-described layout for a dynamic component.
    ///
    /// Overwrites any previous layout for the same id, so a manifest reload
    /// replaces rather than accumulates. Used by the C# backend so managed
    /// components become field-inspectable in the editor.
    pub fn register_dynamic_component_field_layout(
        &mut self,
        component_id: ComponentId,
        fields: Vec<crate::component_registry::ComponentFieldDescriptor>,
    ) {
        self.component_field_layouts
            .insert(component_id, ComponentFieldLayout::Owned(fields));
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
        let factory_ops: HashMap<ComponentId, trait_type_map::ErasedVecStorageOps<dyn Component>> =
            self.storage_factories
                .iter()
                .filter_map(|(component_id, factory)| match factory {
                    StorageFactory::Native(info) => Some((*component_id, info.ops)),
                    StorageFactory::Dynamic(_) => None,
                })
                .collect();

        // Step 2: Refresh every column whose component id has a native factory.
        for archetype in self.archetypes.values_mut() {
            for &component_id in &archetype.component_types {
                let Some(&ops) = factory_ops.get(&component_id) else {
                    continue;
                };
                let Some(type_id) = component_id.native_type_id() else {
                    continue;
                };
                if let Some(column) = archetype.component_storages.get_trait_storage_mut(type_id) {
                    column.refresh_ops(ops);
                }
            }
        }
    }

    /// Register an unmanaged component described by an external language.
    ///
    /// Re-registering an identical layout and name for the same `stable_id`
    /// is idempotent and returns the existing [`ComponentId`].
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DynamicStableIdZero`] for a zero `stable_id`,
    /// [`WorldError::DynamicSizeZero`] for a zero `size`,
    /// [`WorldError::DynamicAlignmentInvalid`] for a zero or non-power-of-two
    /// `align`, [`WorldError::DynamicLayoutInvalid`] for an oversized layout,
    /// [`WorldError::DynamicAlreadyRegistered`] when the `stable_id` is
    /// already taken by a different layout or name, and
    /// [`WorldError::ComponentTypeLimitExceeded`] when the registry is full.
    pub fn register_dynamic_component(
        &mut self,
        stable_id: u128,
        name: impl Into<String>,
        size: usize,
        align: usize,
        schema_hash: u64,
    ) -> Result<ComponentId, WorldError> {
        if stable_id == 0 {
            return Err(WorldError::DynamicStableIdZero);
        }
        if size == 0 {
            return Err(WorldError::DynamicSizeZero);
        }
        if align == 0 || !align.is_power_of_two() {
            return Err(WorldError::DynamicAlignmentInvalid);
        }
        if std::alloc::Layout::from_size_align(size, align).is_err() {
            return Err(WorldError::DynamicLayoutInvalid);
        }
        let name = name.into();
        let component_id = ComponentId::dynamic(stable_id);
        if let Some(existing) = self.storage_factories.get(&component_id) {
            return match existing {
                StorageFactory::Dynamic(layout)
                    if layout.size == size
                        && layout.align == align
                        && layout.schema_hash == schema_hash
                        && self.component_registry.get_name(&component_id) == Some(&name) =>
                {
                    Ok(component_id)
                }
                _ => Err(WorldError::DynamicAlreadyRegistered),
            };
        }
        // The registry reports the 128-type ceiling as a typed error (with the
        // offending name and current count) rather than panicking; propagate it.
        self.component_registry
            .register_dynamic(stable_id, name, size)?;
        self.storage_factories.insert(
            component_id,
            StorageFactory::Dynamic(DynamicComponentLayout {
                size,
                align,
                schema_hash,
            }),
        );
        Ok(component_id)
    }

    /// Return a raw dynamic component column for language bindings.
    pub fn dynamic_component_chunk_mut(
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
        let column = archetype
            .dynamic_component_storages
            .get_mut(&component_id)?;
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
    /// The native twin of [`Self::dynamic_component_chunk_mut`]: returns the
    /// contiguous row buffer of a native (Rust-registered) component as raw
    /// bytes, so the C# backend can expose components that an optional module
    /// registered without naming their concrete Rust type. Only native
    /// components are served; dynamic components must use the dynamic variant.
    /// The returned pointer is only valid for the active managed-system
    /// invocation and must not be retained beyond it.
    pub fn native_component_chunk_mut(
        &mut self,
        component_id: ComponentId,
        chunk_index: usize,
    ) -> Option<(ArchetypeId, *mut u8, usize, usize, &mut [ComponentTicks])> {
        let type_id = component_id.native_type_id()?;
        let archetype = self
            .archetypes
            .values_mut()
            .filter(|archetype| archetype.component_types.contains(&component_id))
            .nth(chunk_index)?;
        let archetype_id = archetype.id;
        let column = archetype
            .component_storages
            .get_trait_storage_mut(type_id)?;
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

    /// Resolve a component ID from its registered type name, without the
    /// persistable-only filter.
    ///
    /// Used by the C# backend to map an optional module's exposed component
    /// name (e.g. `pill_spline::Spline`) to its native [`ComponentId`] so a
    /// byte-level binding can be created without naming the concrete type.
    pub fn resolve_component_id_by_name_any(&self, type_name: &str) -> Option<ComponentId> {
        self.component_registry
            .registered_components()
            .filter(|(_, _, name)| *name == type_name)
            .max_by_key(|(_, bit, _)| *bit)
            .map(|(id, _, _)| id)
    }

    /// Return the byte size and alignment of a registered component's layout.
    ///
    /// Works for both native (Rust) and dynamic (foreign-language) components.
    /// Used by the C# backend to validate that a managed mirror struct has the
    /// same ABI layout as the component an optional module registered.
    pub fn component_layout(&self, component_id: ComponentId) -> Option<(usize, usize)> {
        match self.storage_factories.get(&component_id) {
            Some(StorageFactory::Native(info)) => Some((info.size, info.align)),
            Some(StorageFactory::Dynamic(layout)) => Some((layout.size, layout.align)),
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
    /// Unlike [`Self::resolve_component_id_by_name_any`], which picks the
    /// highest bit across every generation the registry still remembers, this
    /// looks only at the columns the entity really has. That makes it correct
    /// across reloads even when a bit index was recycled, and it guarantees the
    /// returned id has a live column, a live tick vector, and a field layout
    /// belonging to the generation that created the data.
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
    /// Returns [`WorldError::DynamicEntityEmpty`] if `components` is empty,
    /// [`WorldError::DynamicDuplicateComponent`] if a component appears more
    /// than once, [`WorldError::DynamicComponentNotRegistered`] if a
    /// component was never registered, and
    /// [`WorldError::DynamicByteLengthMismatch`] if a byte payload does not
    /// match its registered layout size.
    pub fn create_dynamic_entity(
        &mut self,
        components: &[(ComponentId, Vec<u8>)],
    ) -> Result<Entity, WorldError> {
        // Step 1: Validate the component set - non-empty, unique, registered,
        // and every payload matches its registered layout.
        if components.is_empty() {
            return Err(WorldError::DynamicEntityEmpty);
        }
        let mut component_ids: Vec<_> = components.iter().map(|(id, _)| *id).collect();
        component_ids.sort();
        component_ids.dedup();
        if component_ids.len() != components.len() {
            return Err(WorldError::DynamicDuplicateComponent);
        }
        for (id, bytes) in components {
            let Some(StorageFactory::Dynamic(layout)) = self.storage_factories.get(id) else {
                return Err(WorldError::DynamicComponentNotRegistered { id: *id });
            };
            if bytes.len() != layout.size {
                return Err(WorldError::DynamicByteLengthMismatch { id: *id });
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
            if !archetype.dynamic_component_storages.contains_key(id)
                || !archetype.component_ticks.contains_key(id)
            {
                return Err(WorldError::DynamicStorageMissing {
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
            match archetype.dynamic_component_storages.get_mut(id) {
                Some(storage) => storage.push_bytes(bytes)?,
                None => {
                    return Err(WorldError::DynamicStorageMissing {
                        component_id: *id,
                        archetype_id,
                    })
                }
            }
            match archetype.component_ticks.get_mut(id) {
                Some(ticks) => ticks.push(ComponentTicks::new(current_tick)),
                None => {
                    return Err(WorldError::DynamicStorageMissing {
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
    pub fn dynamic_component_bytes(
        &self,
        entity: Entity,
        component_id: ComponentId,
    ) -> Option<&[u8]> {
        let location = self.entity_locations.get(&entity)?;
        self.archetypes
            .get(&location.archetype_id)?
            .dynamic_component_storages
            .get(&component_id)?
            .bytes(location.index_in_archetype)
    }

    /// Register a script component type with the World
    ///
    /// Script components have an update() method that gets called by update_scripts().
    /// This must be called for each script component type before it can be used.
    pub fn register_script_component<T>(&mut self)
    where
        T: ScriptComponent + TraitAccessible<dyn Component> + Clone,
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
                (|storage: &mut TraitTypeMap<dyn Component, VecFamily>,
                  index: usize,
                  entity: Entity,
                  world_ptr: *mut World,
                  commands_ptr: *mut CommandQueue| {
                    // Get mutable reference to the component
                    let component = storage.get_storage_mut::<T>().get_mut::<T>(index);
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
    #[inline]
    pub fn increment_change_tick(&mut self) -> Tick {
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
    pub fn insert_resource<T: Resource>(&mut self, resource: T) {
        let _zone = crate::profile_scope!(
            "insert resource",
            [(
                "Resource type being inserted: {}",
                std::any::type_name::<T>()
            )]
        );
        let id = ResourceId::of::<T>();
        let tick = Tick::new(self.change_tick);
        self.resources.insert(id, Box::new(resource));
        self.resource_ticks.insert(id, ComponentTicks::new(tick));
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
            .and_then(|boxed| boxed.downcast_ref::<T>())
    }

    /// Get mutable reference to a resource.
    ///
    /// Prefer [`get_resource_mut_tracked`] for system-parameter usage so
    /// that change-detection ticks are automatically bumped on mutation.
    pub fn get_resource_mut<T: Resource>(&mut self) -> Option<&mut T> {
        self.resources
            .get_mut(&ResourceId::of::<T>())
            .and_then(|boxed| boxed.downcast_mut::<T>())
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
        #[cfg(debug_assertions)]
        {
            let id = ResourceId::of::<T>();
            debug_assert!(
                !self.debug_resource_write_locks.contains(&id),
                "Resource {:?} is already mutably borrowed - possible scheduler bug or concurrent system access",
                id
            );
            self.debug_resource_write_locks.insert(id);
        }

        let id = ResourceId::of::<T>();
        let value: &mut T = self
            .resources
            .get_mut(&id)
            .and_then(|boxed| boxed.downcast_mut::<T>())?;
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

    /// Remove a resource and return it if it existed
    pub fn remove_resource<T: Resource>(&mut self) -> Option<T> {
        let id = ResourceId::of::<T>();
        self.resource_ticks.remove(&id);
        self.resources
            .remove(&id)
            .and_then(|boxed| boxed.downcast::<T>().ok())
            .map(|boxed| *boxed)
    }

    /// Check if a resource exists
    #[must_use]
    pub fn has_resource<T: Resource>(&self) -> bool {
        self.resources.contains_key(&ResourceId::of::<T>())
    }

    /// Debug-only: Clear the set of mutably-borrowed resources.
    ///
    /// Called by [`Engine::process_frame`] at the start of every frame so
    /// that the isolation check only guards against concurrent access
    /// within a single frame.
    #[cfg(debug_assertions)]
    pub(crate) fn debug_clear_resource_locks(&mut self) {
        self.debug_resource_write_locks.clear();
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
        T: Component + TraitAccessible<dyn Component>,
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
                .get_storage::<T>()
                .get::<T>(location.index_in_archetype),
        )
    }

    /// Get mutable reference to a component on an entity
    ///
    /// Returns None if the entity doesn't exist or doesn't have the component.
    pub fn get_component_mut<T>(&mut self, entity: Entity) -> Option<&mut T>
    where
        T: Component + TraitAccessible<dyn Component>,
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
                .get_storage_mut::<T>()
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
        T: Component + TraitAccessible<dyn Component>,
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
        let storage = archetype.component_storages.get_storage_mut::<T>();
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

        // Hot path: archetype already exists (most common case)
        if self.archetypes.contains_key(&archetype_id) {
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
    /// Note: With TraitTypeMap, we need concrete types to push components.
    /// Components are added via EntityBuilder which has access to concrete types.
    pub(crate) fn insert_entity_with_components<F>(
        &mut self,
        entity: Entity,
        component_ids: Vec<ComponentId>,
        insert_fn: F,
    ) where
        F: FnOnce(&mut TraitTypeMap<dyn Component, VecFamily>),
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
            if let Some(column) = archetype.dynamic_component_storages.get_mut(&component_id) {
                column.push_zeroed();
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
    /// [`WorldError::DynamicStorageMissing`] when a dynamic component named
    /// by an archetype has no storage column — the desync a partially applied
    /// hot reload can leave behind.
    pub(crate) fn move_entity_to_archetype<F>(
        &mut self,
        entity: Entity,
        new_component_ids: Vec<ComponentId>,
        move_fn: F,
    ) -> Result<(), WorldError>
    where
        F: FnOnce(
            &TraitTypeMap<dyn Component, VecFamily>,
            &mut TraitTypeMap<dyn Component, VecFamily>,
            usize,
        ),
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
                let Some(destination) = new_archetype
                    .dynamic_component_storages
                    .get_mut(&component_id)
                else {
                    // Native components have no dynamic column and are skipped
                    // by design. A dynamic component (whose native_type_id is
                    // None) missing its column is a manifest/storage desync -
                    // the condition `WorldError::DynamicStorageMissing`
                    // reports - so fail the migration rather than leave the
                    // destination archetype short a column.
                    if component_id.native_type_id().is_none() {
                        return Err(WorldError::DynamicStorageMissing {
                            component_id,
                            archetype_id: new_archetype_id,
                        });
                    }
                    continue;
                };
                if let Some(source) = old_archetype.dynamic_component_storages.get(&component_id) {
                    destination.push_from(source, old_index);
                } else {
                    destination.push_zeroed();
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
                match component_id.native_type_id() {
                    Some(type_id) => {
                        if let Some(storage) = old_archetype
                            .component_storages
                            .get_trait_storage_mut(type_id)
                        {
                            storage.swap_remove_discard(old_index);
                        }
                    }
                    None => {
                        let Some(column) = old_archetype
                            .dynamic_component_storages
                            .get_mut(&component_id)
                        else {
                            // A dynamic component without a column is the
                            // manifest/storage desync this function already
                            // reports during migration; surface it here too
                            // instead of panicking mid-frame.
                            return Err(WorldError::DynamicStorageMissing {
                                component_id,
                                archetype_id: old_archetype_id,
                            });
                        };
                        column.swap_remove(old_index);
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
                match component_id.native_type_id() {
                    Some(type_id) => {
                        if let Some(storage) =
                            archetype.component_storages.get_trait_storage_mut(type_id)
                        {
                            storage.swap_remove_discard(old_index);
                        }
                    }
                    None => {
                        if let Some(column) =
                            archetype.dynamic_component_storages.get_mut(component_id)
                        {
                            column.swap_remove(old_index);
                        } else {
                            // A dynamic component with no column has no data to
                            // remove, so skipping is safe. The missing column is
                            // the manifest/storage desync reported by
                            // `WorldError::DynamicStorageMissing`; report rather
                            // than panic, because this runs inside
                            // `process_frame` for managed projects.
                            warn!(
                                target: pill_core::telemetry::telemetry_target::ECS,
                                component_id = ?component_id,
                                archetype_id = ?archetype.id,
                                "destroy_entity: dynamic component has no storage column; skipping"
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
            // startable; the dynamic paths propagate the same failure as a
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
        T: Component + TraitAccessible<dyn Component> + Clone,
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
                new_storage.get_storage_mut::<T>().push::<T>(component);
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
    /// [`WorldError::DynamicComponentAlreadyPresent`] if the entity already
    /// carries the component, [`WorldError::DynamicComponentNotRegistered`]
    /// if the component was never registered, and
    /// [`WorldError::DynamicByteLengthMismatch`] if `bytes` does not match
    /// the registered layout size.
    pub fn add_dynamic_component(
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
            return Err(WorldError::DynamicComponentAlreadyPresent);
        }
        let expected_size = match self.storage_factories.get(&component_id) {
            Some(StorageFactory::Dynamic(layout)) => layout.size,
            _ => return Err(WorldError::DynamicComponentNotRegistered { id: component_id }),
        };
        if bytes.len() != expected_size {
            return Err(WorldError::DynamicByteLengthMismatch { id: component_id });
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
        let Some(storage) = archetype.dynamic_component_storages.get_mut(&component_id) else {
            return Err(WorldError::DynamicStorageMissing {
                component_id,
                archetype_id: location.archetype_id,
            });
        };
        storage.set_bytes(location.index_in_archetype, bytes)?;
        Ok(())
    }

    /// Replace the bytes of an existing runtime-defined component row.
    pub(crate) fn set_dynamic_component_bytes(
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
            .and_then(|archetype| archetype.dynamic_component_storages.get_mut(&component_id))
            .ok_or(WorldError::DynamicComponentMissing)?
            .set_bytes(location.index_in_archetype, bytes)
    }

    /// Add a zero-initialized runtime-defined component.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DynamicComponentNotRegistered`] if the component
    /// was never registered, plus any error reported by
    /// [`add_dynamic_component`](Self::add_dynamic_component).
    pub fn add_dynamic_component_default(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
    ) -> Result<(), WorldError> {
        let size = match self.storage_factories.get(&component_id) {
            Some(StorageFactory::Dynamic(layout)) => layout.size,
            _ => return Err(WorldError::DynamicComponentNotRegistered { id: component_id }),
        };
        self.add_dynamic_component(entity, component_id, &vec![0; size])
    }

    /// Remove a runtime-defined component while preserving every other column.
    ///
    /// If the entity's last component is removed, the entity is destroyed
    /// instead of migrated to an empty archetype.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::EntityNotFound`] if the entity does not exist,
    /// and [`WorldError::DynamicComponentMissing`] if the entity does not
    /// carry the component.
    pub fn remove_dynamic_component(
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
        if !old_archetype
            .dynamic_component_storages
            .contains_key(&component_id)
        {
            return Err(WorldError::DynamicComponentMissing);
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
    fn insert(self: Box<Self>, storage: &mut TraitTypeMap<dyn Component, VecFamily>);
    /// Return the [`ComponentId`] of the captured component type.
    fn component_id(&self) -> ComponentId;
}

/// Implementation of [`ComponentInserter`] that captures a concrete component type.
struct TypedComponentInserter<T: Component + TraitAccessible<dyn Component>> {
    /// The component value to insert when the entity is built.
    component: T,
}

impl<T: Component + TraitAccessible<dyn Component>> ComponentInserter
    for TypedComponentInserter<T>
{
    fn insert(self: Box<Self>, storage: &mut TraitTypeMap<dyn Component, VecFamily>) {
        storage.get_storage_mut::<T>().push::<T>(self.component);
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
        T: Component + TraitAccessible<dyn Component>,
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
fn copy_component<T: Component + TraitAccessible<dyn Component> + Clone>(
    source: &TraitTypeMap<dyn Component, VecFamily>,
    destination: &mut TraitTypeMap<dyn Component, VecFamily>,
    index: usize,
) {
    let component = source.get_storage::<T>().get::<T>(index);
    destination
        .get_storage_mut::<T>()
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
    use trait_type_map::impl_trait_accessible;

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

    impl_trait_accessible!(dyn Component; Position, Velocity, Health);

    #[test]
    fn dynamic_components_coexist_and_survive_archetype_migration() {
        let mut world = World::new();
        let a = world
            .register_dynamic_component(0xA1, "Project.A", 4, 4, 11)
            .unwrap();
        let b = world
            .register_dynamic_component(0xB2, "Project.B", 4, 4, 22)
            .unwrap();
        let c = world
            .register_dynamic_component(0xC3, "Project.C", 8, 8, 33)
            .unwrap();
        let entity = world
            .create_dynamic_entity(&[
                (a, 10_u32.to_ne_bytes().to_vec()),
                (b, 20_u32.to_ne_bytes().to_vec()),
            ])
            .unwrap();

        assert_eq!(
            world.dynamic_component_bytes(entity, a).unwrap(),
            10_u32.to_ne_bytes()
        );
        assert_eq!(
            world.dynamic_component_bytes(entity, b).unwrap(),
            20_u32.to_ne_bytes()
        );

        world.add_dynamic_component_default(entity, c).unwrap();
        assert_eq!(
            world.dynamic_component_bytes(entity, a).unwrap(),
            10_u32.to_ne_bytes()
        );
        assert_eq!(
            world.dynamic_component_bytes(entity, b).unwrap(),
            20_u32.to_ne_bytes()
        );
        assert_eq!(world.dynamic_component_bytes(entity, c).unwrap(), [0; 8]);

        world.remove_dynamic_component(entity, b).unwrap();
        assert_eq!(
            world.dynamic_component_bytes(entity, a).unwrap(),
            10_u32.to_ne_bytes()
        );
        assert!(world.dynamic_component_bytes(entity, b).is_none());
        assert_eq!(world.dynamic_component_bytes(entity, c).unwrap(), [0; 8]);
    }

    #[test]
    fn dynamic_component_ticks_survive_archetype_migration() {
        fn ticks_for(world: &World, entity: Entity, component: ComponentId) -> ComponentTicks {
            let location = world.entity_locations[&entity];
            world.archetypes[&location.archetype_id].component_ticks[&component]
                [location.index_in_archetype]
        }

        let mut world = World::new();
        let retained = world
            .register_dynamic_component(0xA1, "Project.Retained", 4, 4, 11)
            .unwrap();
        let removed = world
            .register_dynamic_component(0xB2, "Project.Removed", 4, 4, 22)
            .unwrap();
        let added = world
            .register_dynamic_component(0xC3, "Project.Added", 8, 8, 33)
            .unwrap();
        let entity = world
            .create_dynamic_entity(&[
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
        world.add_dynamic_component_default(entity, added).unwrap();

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
        world.remove_dynamic_component(entity, removed).unwrap();

        let retained_after_remove = ticks_for(&world, entity, retained);
        let added_after_remove = ticks_for(&world, entity, added);
        assert_eq!(retained_after_remove.added, retained_after_add.added);
        assert_eq!(retained_after_remove.changed, retained_after_add.changed);
        assert_eq!(added_after_remove.added, added_after_add.added);
        assert_eq!(added_after_remove.changed, added_after_add.changed);
        assert!(world.dynamic_component_bytes(entity, removed).is_none());
    }

    /// The native byte-chunk accessor exposes a native column's rows as raw
    /// bytes with the correct element size, mirroring the dynamic path used by
    /// the C# backend for optional-module components. Dynamic components are
    /// rejected by it.
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

        // Dynamic components are served by the dynamic accessor, not this one.
        let dynamic = world
            .register_dynamic_component(0xD1, "NativeChunkTest.Dynamic", 4, 4, 1)
            .unwrap();
        assert!(world.native_component_chunk_mut(dynamic, 0).is_none());
        // An unknown native id is rejected too.
        let unknown = world
            .register_dynamic_component(0xD2, "NativeChunkTest.Unknown", 4, 4, 2)
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

    #[test]
    fn invalid_dynamic_component_layouts_are_rejected() {
        let mut world = World::new();
        assert!(world
            .register_dynamic_component(1, "Zero", 0, 1, 0)
            .is_err());
        assert!(world
            .register_dynamic_component(2, "BadAlign", 4, 3, 0)
            .is_err());
        assert!(world
            .register_dynamic_component(4, "Oversized", usize::MAX, 1, 0)
            .is_err());
        world
            .register_dynamic_component(3, "SchemaA", 4, 4, 10)
            .unwrap();
        assert!(world
            .register_dynamic_component(3, "SameSchemaDifferentName", 4, 4, 10)
            .is_err());
        assert!(world
            .register_dynamic_component(3, "SchemaB", 8, 8, 20)
            .is_err());

        let valid = world
            .register_dynamic_component(5, "Valid", 4, 4, 30)
            .unwrap();
        assert!(world.create_dynamic_entity(&[(valid, vec![0; 3])]).is_err());
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

    /// `entity_component_names` includes runtime-defined (dynamic) components,
    /// and `resolve_entity_component_id` is scoped to the entity's archetype.
    #[test]
    fn dynamic_components_appear_and_resolution_is_archetype_scoped() {
        let mut world = World::new();
        world.register_component::<Position>();
        let dynamic = world
            .register_dynamic_component(0xABCD, "Demo.Thing", 4, 4, 99)
            .unwrap();

        let with_dynamic = world
            .create_dynamic_entity(&[(dynamic, 7_u32.to_ne_bytes().to_vec())])
            .unwrap();
        let with_position = world
            .create_entity()
            .with(Position { x: 0.0, y: 0.0 })
            .build()
            .unwrap();

        let names = world
            .entity_component_names(with_dynamic)
            .expect("entity alive");
        assert_eq!(names, vec!["Demo.Thing"]);

        // Resolution is per-entity: the dynamic component only resolves on the
        // entity that carries it, and Position only on its own entity.
        assert_eq!(
            world.resolve_entity_component_id(with_dynamic, "Demo.Thing"),
            Some(dynamic)
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

    /// `registered_components` lists every type (native and dynamic), sorted.
    #[test]
    fn registered_components_lists_every_type_sorted() {
        let mut world = World::new();
        world.register_component::<Velocity>();
        world.register_component::<Position>();
        world
            .register_dynamic_component(0x1111, "Demo.Alpha", 4, 4, 1)
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
}
