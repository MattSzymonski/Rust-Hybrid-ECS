//! Archetype-based component storage with SoA layout.
//!
//! An archetype is a unique combination of component types. All entities with
//! the same set of components are stored together in the same archetype for
//! cache-friendly iteration.
//!
//! # Responsibilities
//!
//! - Group entities by their exact component set into [`Archetype`] instances.
//! - Own the contiguous, type-erased component storage for each archetype,
//!   both native (`TraitTypeMap`) and dynamically laid out ([`DynamicColumn`]).
//! - Track change-detection [`ComponentTicks`] for every component instance.
//!
//! # Design
//!
//! Archetypes use a Structure of Arrays (SoA) layout rather than an
//! Array of Structures (AoS). This means components of the same type are
//! stored contiguously in memory:
//!
//! ```text
//! Archetype [Position, Velocity]
//! ┌─────────────────────────────────────────────────┐
//! │ Entities:    [E1,     E2,     E3,     E4    ]   │
//! │ Positions:   [Pos1,   Pos2,   Pos3,   Pos4  ]   │
//! │ Velocities:  [Vel1,   Vel2,   Vel3,   Vel4  ]   │
//! └─────────────────────────────────────────────────┘
//! ```
//!
//! When iterating over all entities with Position+Velocity:
//! - SoA (this design): Sequential memory access, excellent cache utilization
//! - AoS alternative: Scattered access, poor cache performance
//!
//! The tradeoff is that accessing all components of a single entity requires
//! multiple array lookups, but this is rare compared to bulk iteration
//! in ECS-style approaches.
//!
//! Entity removal uses swap-remove to keep arrays dense: the removed entity
//! is swapped with the last entity in each component array, the last element
//! (now the removed entity's data) is popped, and the swapped entity's
//! location is updated in the `entity_locations` map. This keeps arrays
//! dense without gaps, maintaining O(1) removal.

// Standard library
use std::alloc::{alloc, dealloc, handle_alloc_error, Layout};
use std::collections::HashMap;
use std::ptr::NonNull;

// External crates
use trait_type_map::{ErasedVecStorage, ErasedVecStorageInfo, TraitTypeMap, VecFamily};

// Current crate
use crate::component::{Component, ComponentId, ComponentMask, ComponentTicks};
use crate::entity::Entity;
use crate::error::WorldError;

// =============================================================================
// StorageFactory
// =============================================================================

/// Strategy for creating component storage for a specific component type.
///
/// Registered per [`ComponentId`] and consulted when an [`Archetype`] is
/// created so the archetype can allocate storage without knowing the
/// concrete component types.
pub enum StorageFactory {
    /// Creates a type-erased native storage column inside the archetype's
    /// [`TraitTypeMap`].
    ///
    /// Carries only data (type id, layout, per-type function table), never a
    /// closure: the column is stored as a concrete `Box<ErasedVecStorage>`
    /// with no trait-object vtable, so it survives module unloads; the engine
    /// refreshes its function table on every reload.
    Native(ErasedVecStorageInfo<dyn Component>),
    /// Carries the runtime layout of a component owned by another language.
    Dynamic(DynamicComponentLayout),
}

// =============================================================================
// DynamicComponentLayout
// =============================================================================

/// Runtime layout for a component whose concrete type is owned by another language.
///
/// Describes the memory footprint of an opaque component column so its rows
/// can be copied in and out as raw bytes without knowing the concrete type.
#[derive(Debug, Clone)]
pub struct DynamicComponentLayout {
    /// Size in bytes of a single component instance.
    pub size: usize,
    /// Alignment in bytes required by a single component instance.
    pub align: usize,
    /// Hash identifying the component's schema across language boundaries.
    pub schema_hash: u64,
}

// =============================================================================
// DynamicColumn
// =============================================================================

/// Aligned, densely packed storage for a type-erased component column.
///
/// Owns a raw heap allocation whose element size and alignment come from a
/// [`DynamicComponentLayout`]. Rows are written and read as raw bytes, which
/// lets components defined in other languages share storage with native ones.
pub struct DynamicColumn {
    /// Runtime layout of the stored element type.
    layout: DynamicComponentLayout,
    /// Pointer to the heap allocation, or dangling before the first growth.
    data: NonNull<u8>,
    /// Number of initialized rows.
    len: usize,
    /// Number of rows the current allocation can hold.
    capacity: usize,
}

impl DynamicColumn {
    /// Creates an empty column for the given runtime layout.
    ///
    /// No heap allocation is made until the first row is pushed.
    ///
    /// This is a POD-only container: rows are copied as raw bytes and the
    /// buffer is freed without running element destructors, so the layout
    /// must describe a blittable value type. `pill_host::csharp::components`
    /// enforces that through `BLITTABLE_FIELD_TYPES` before any layout is
    /// registered; the debug assertion below is a second line of defense for
    /// any caller that constructs a layout directly.
    pub fn new(layout: DynamicComponentLayout) -> Self {
        debug_assert!(
            layout.size > 0
                && layout.align > 0
                && layout.align.is_power_of_two()
                && std::alloc::Layout::from_size_align(layout.size, layout.align).is_ok(),
            "invalid DynamicComponentLayout: size {} align {} (must be a valid, \
             non-zero POD layout)",
            layout.size,
            layout.align
        );
        Self {
            layout,
            data: NonNull::dangling(),
            len: 0,
            capacity: 0,
        }
    }

    /// Returns the number of initialized rows in this column.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether this column holds no rows.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the size in bytes of a single stored component instance.
    pub fn element_size(&self) -> usize {
        self.layout.size
    }

    /// Returns the alignment in bytes required by a single stored instance.
    pub fn alignment(&self) -> usize {
        self.layout.align
    }

    /// Returns the schema hash identifying the stored component type.
    pub fn schema_hash(&self) -> u64 {
        self.layout.schema_hash
    }

    /// Returns a raw pointer to the underlying buffer.
    ///
    /// The pointer is dangling before the first allocation and is invalidated
    /// by any later reallocation or by dropping the column.
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.data.as_ptr()
    }

    /// Appends a zero-initialized row to this column.
    ///
    /// Grows the column when needed and leaves the new row's bytes zeroed.
    pub fn push_zeroed(&mut self) {
        self.reserve_one();
        // SAFETY: reserve_one guarantees one writable, correctly aligned slot.
        unsafe {
            std::ptr::write_bytes(
                self.data.as_ptr().add(self.len * self.layout.size),
                0,
                self.layout.size,
            )
        };
        self.len += 1;
    }

    /// Appends a row containing a copy of the given bytes.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DynamicSizeMismatch`] when `bytes` does not
    /// contain exactly `element_size()` bytes.
    pub fn push_bytes(&mut self, bytes: &[u8]) -> Result<(), WorldError> {
        if bytes.len() != self.layout.size {
            return Err(WorldError::DynamicSizeMismatch);
        }
        self.reserve_one();
        // SAFETY: source and destination are valid for exactly one element and
        // cannot overlap because the source is outside this column's spare slot.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.data.as_ptr().add(self.len * self.layout.size),
                self.layout.size,
            );
        }
        self.len += 1;
        Ok(())
    }

    /// Copies the row at `index` from another column and appends it here.
    ///
    /// # Panics
    ///
    /// Panics when the two columns have different element sizes or when
    /// `index` is out of bounds of `source`.
    pub fn push_from(&mut self, source: &Self, index: usize) {
        assert_eq!(self.layout.size, source.layout.size);
        assert!(index < source.len);
        self.reserve_one();
        // SAFETY: both slots are allocated, aligned, non-overlapping columns.
        unsafe {
            std::ptr::copy_nonoverlapping(
                source.data.as_ptr().add(index * source.layout.size),
                self.data.as_ptr().add(self.len * self.layout.size),
                self.layout.size,
            );
        }
        self.len += 1;
    }

    /// Overwrites the row at `index` with a copy of the given bytes.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DynamicRowInvalid`] when `index` is out of
    /// bounds or `bytes` does not contain exactly `element_size()` bytes.
    pub fn set_bytes(&mut self, index: usize, bytes: &[u8]) -> Result<(), WorldError> {
        if index >= self.len || bytes.len() != self.layout.size {
            return Err(WorldError::DynamicRowInvalid);
        }
        // SAFETY: the checked row is initialized and bytes has one element's size.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.data.as_ptr().add(index * self.layout.size),
                self.layout.size,
            );
        }
        Ok(())
    }

    /// Returns the raw bytes of the row at `index`, or `None` when out of bounds.
    pub fn bytes(&self, index: usize) -> Option<&[u8]> {
        if index >= self.len {
            return None;
        }
        // SAFETY: the row is initialized and lives for the returned shared borrow.
        Some(unsafe {
            std::slice::from_raw_parts(
                self.data.as_ptr().add(index * self.layout.size),
                self.layout.size,
            )
        })
    }

    /// Removes the row at `index` by swapping in the last row.
    ///
    /// Keeps the column dense and runs in O(1), but does not preserve row
    /// ordering.
    ///
    /// # Panics
    ///
    /// Panics when `index` is out of bounds.
    pub fn swap_remove(&mut self, index: usize) {
        assert!(index < self.len);
        let last = self.len - 1;
        if index != last {
            // SAFETY: both rows are within this allocation; copy permits overlap.
            unsafe {
                std::ptr::copy(
                    self.data.as_ptr().add(last * self.layout.size),
                    self.data.as_ptr().add(index * self.layout.size),
                    self.layout.size,
                );
            }
        }
        self.len = last;
    }

    /// Ensures capacity for at least one more row, growing the buffer when full.
    fn reserve_one(&mut self) {
        // Step 1: Early-exit when the column still has a spare slot.
        if self.len < self.capacity {
            return;
        }

        // Step 2: Compute the doubled capacity and the layout it requires.
        //
        // The floor is applied *after* doubling. Applying it before made the
        // first allocation eight rows rather than four, over-allocating every
        // manifest-driven column on first use.
        let new_capacity = self.capacity.checked_mul(2).unwrap_or(4).max(4);
        let new_layout = Layout::from_size_align(
            self.layout
                .size
                .checked_mul(new_capacity)
                .expect("dynamic column too large"),
            self.layout.align,
        )
        .expect("invalid dynamic component layout");

        // Step 3: Allocate the new buffer and migrate the existing rows.
        // SAFETY: new_layout has non-zero size because component size is validated.
        let new_data = unsafe { alloc(new_layout) };
        let new_data = NonNull::new(new_data).unwrap_or_else(|| handle_alloc_error(new_layout));
        if self.len != 0 {
            // SAFETY: both allocations are valid and non-overlapping.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.data.as_ptr(),
                    new_data.as_ptr(),
                    self.len * self.layout.size,
                );
                dealloc(
                    self.data.as_ptr(),
                    Layout::from_size_align_unchecked(
                        self.layout.size * self.capacity,
                        self.layout.align,
                    ),
                );
            }
        }

        // Step 4: Adopt the new allocation as the column's buffer.
        self.data = new_data;
        self.capacity = new_capacity;
    }
}

// SAFETY: Two premises, each enforced by named code rather than asserted here.
//
// 1. Every field of a dynamic component is a blittable value type, enforced by
//    `BLITTABLE_FIELD_TYPES` in `pill_host::csharp::components`, which rejects
//    any manifest declaring a managed reference. This is what makes the raw
//    `ptr::copy` in `swap_remove` and the destructor-free `Drop` below correct:
//    there is no ownership to duplicate or release.
// 2. Access is serialised by the same scheduler rules as native columns - see
//    `SystemAccess::conflicts_with`, which only takes its bitmask fast path when
//    both systems' access masks are complete.
unsafe impl Send for DynamicColumn {}
unsafe impl Sync for DynamicColumn {}

impl Drop for DynamicColumn {
    /// Frees the buffer. **No element destructor runs, by design.**
    ///
    /// Every field of a dynamic component is a blittable value type - enforced
    /// by `BLITTABLE_FIELD_TYPES` in `pill_host::csharp::components`, which
    /// rejects any manifest declaring otherwise - so a row owns nothing that
    /// needs releasing. That is also what makes the raw `ptr::copy` in
    /// `swap_remove` correct: moving a row cannot duplicate ownership.
    ///
    /// If dynamic components ever need to own a resource, this is the first
    /// place that has to change: `DynamicComponentLayout` would need an
    /// optional `drop_fn`, called here and from `swap_remove`.
    fn drop(&mut self) {
        if self.capacity != 0 {
            // SAFETY: this is the live allocation created by reserve_one.
            unsafe {
                dealloc(
                    self.data.as_ptr(),
                    Layout::from_size_align_unchecked(
                        self.layout.size * self.capacity,
                        self.layout.align,
                    ),
                );
            }
        }
    }
}

// =============================================================================
// ArchetypeId
// =============================================================================

/// ArchetypeId uniquely identifies an archetype by its component mask.
///
/// Derived from the archetype's [`ComponentMask`], guaranteeing a 1:1
/// mapping without a separate lookup table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArchetypeId(
    /// The packed component mask bits identifying this archetype.
    pub u128,
);

// =============================================================================
// Archetype
// =============================================================================

/// Core storage unit grouping entities with the same component set.
///
/// Uses a Structure of Arrays (SoA) layout for cache-friendly bulk iteration.
pub struct Archetype {
    /// Unique identifier derived from this archetype's component mask.
    pub id: ArchetypeId,
    /// Component types stored in this archetype, in registration order.
    ///
    /// Still needed for iteration and lookup.
    pub component_types: Vec<ComponentId>,
    /// Bitmask of the stored component types for fast query matching.
    pub component_mask: ComponentMask,
    /// Type-erased native component storage, keyed by component type.
    pub component_storages: TraitTypeMap<dyn Component, VecFamily>,
    /// Byte-oriented storage for components owned by other languages.
    pub dynamic_component_storages: HashMap<ComponentId, DynamicColumn>,
    /// Entities currently stored in this archetype.
    pub entities: Vec<Entity>,
    /// Per-component-instance change-detection metadata.
    ///
    /// For each `ComponentId` in `component_types`, the matching
    /// `Vec<ComponentTicks>` is kept in lockstep with the underlying
    /// component storage: row `i` of the tick vec corresponds to row `i`
    /// of the component vec for the same entity. Maintenance happens in
    /// `World` whenever entities are inserted, moved between archetypes,
    /// or destroyed.
    pub component_ticks: HashMap<ComponentId, Vec<ComponentTicks>>,
}

impl Archetype {
    /// Creates a new archetype with storage for the specified component types.
    ///
    /// `storage_factories` provides a way to create storage for each component
    /// type by [`ComponentId`], allowing archetype creation without knowing the
    /// concrete types.
    ///
    /// # Panics
    ///
    /// Panics when a component type has no entry in `storage_factories`, which
    /// happens when `world.register_component::<T>()` was not called for it.
    pub fn new(
        id: ArchetypeId,
        component_types: Vec<ComponentId>,
        component_mask: ComponentMask,
        storage_factories: &HashMap<ComponentId, StorageFactory>,
    ) -> Self {
        // Step 1: Pre-size the storage maps for the component count.
        let component_count = component_types.len();
        let _zone = crate::profile_scope!(
            "archetype new",
            [("Component types in this archetype: {}", component_count)]
        );
        let mut component_storages = TraitTypeMap::with_capacity(component_count);
        let mut dynamic_component_storages = HashMap::new();
        let mut component_ticks: HashMap<ComponentId, Vec<ComponentTicks>> =
            HashMap::with_capacity(component_count);

        // Step 2: Create storage for each component type using its factory.
        for &component_id in &component_types {
            let factory = storage_factories.get(&component_id)
                .unwrap_or_else(|| panic!(
                    "Component type {:?} not registered. Call world.register_component::<T>() first.",
                    component_id
                ));
            match factory {
                StorageFactory::Native(info) => {
                    // Build the erased column from the registered type
                    // description and store it as a concrete
                    // `Box<ErasedVecStorage>` (no trait-object vtable), so
                    // the column stays valid across module unloads.
                    component_storages.insert_erased(ErasedVecStorage::<dyn Component>::new(*info));
                }
                StorageFactory::Dynamic(layout) => {
                    dynamic_component_storages
                        .insert(component_id, DynamicColumn::new(layout.clone()));
                }
            }
            component_ticks.insert(component_id, Vec::new());
        }

        // Step 3: Emit allocation telemetry and assemble the archetype.
        crate::profile_message!(
            "archetype {:?} allocated with {} component storage columns for up to 0 entities",
            id,
            component_count,
        );

        Self {
            id,
            component_types,
            component_mask,
            component_storages,
            dynamic_component_storages,
            entities: Vec::new(),
            component_ticks,
        }
    }

    /// Checks whether this archetype stores the component type at the given mask bit.
    ///
    /// Uses the bitmask for O(1) lookup instead of a linear search through
    /// the component types.
    #[inline]
    pub fn has_component_bit(&self, bit: u8) -> bool {
        self.component_mask.has_bit(bit)
    }

    /// Checks whether this archetype stores the specified component type.
    ///
    /// Note: This uses an O(n) linear search. Prefer `has_component_bit` with
    /// a pre-looked-up bit index for hot paths.
    pub fn has_component<T: Component>(&self) -> bool {
        self.component_types.contains(&ComponentId::of::<T>())
    }

    /// Checks whether this archetype contains every component a query requires.
    ///
    /// Called in the query setup hot path for every archetype.
    #[inline]
    pub fn matches_mask(&self, required_mask: &ComponentMask) -> bool {
        self.component_mask.contains_all(required_mask)
    }

    /// Returns the number of entities in this archetype.
    #[inline]
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    /// Returns the number of entities in this archetype (alias for [`Self::len`]).
    ///
    /// Provided for API consistency with other collection types.
    #[inline]
    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    /// Returns `true` when this archetype contains no entities.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// Builds a human-readable summary of this archetype.
    ///
    /// Includes the archetype ID, the entity count, and the names of the
    /// stored component types. Unknown component IDs render as "Unknown".
    pub fn get_archetype_info(&self, registry: &crate::component::ComponentRegistry) -> String {
        let component_names: Vec<String> = self
            .component_types
            .iter()
            .map(|component_id| {
                registry
                    .get_name(component_id)
                    .unwrap_or("Unknown")
                    .to_string()
            })
            .collect();

        format!(
            "Archetype {:?}: {} entities, components: [{}]",
            self.id,
            self.entities.len(),
            component_names.join(", ")
        )
    }

    /// Prints information about this archetype (component names and entity count).
    #[cold]
    pub fn print_info(&self, registry: &crate::component::ComponentRegistry) {
        let info = self.get_archetype_info(registry);
        println!("{}", info);
    }

    /// Estimate the memory footprint of this archetype in bytes.
    ///
    /// Sums entity IDs, component column capacities x element sizes,
    /// and change-detection tick vectors.
    pub fn memory_estimate(&self, registry: &crate::component::ComponentRegistry) -> usize {
        // Step 1: Account for the entity ID array (16 bytes per entity).
        let mut total = self.entities.capacity() * std::mem::size_of::<crate::entity::Entity>();

        // Step 2: Account for each component column (entity count as a lower bound).
        for &component_id in &self.component_types {
            if let Some(size) = registry.get_size(&component_id) {
                // We can't inspect the Vec's capacity through the trait object,
                // so we use entity count as a lower bound. The actual Vec capacity
                // may be larger due to pre-allocation.
                total += self.entities.len() * size;
            }
        }

        // Step 3: Account for the change-detection tick vectors (8 bytes per tick).
        total += self.component_types.len() * self.entities.capacity() * 8;

        // Step 4: Approximate the HashMap overhead for the tick lookup.
        total += self.component_types.len() * 64;

        total
    }
}
