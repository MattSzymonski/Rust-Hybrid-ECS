//! Archetype-based component storage with SoA layout.
//!
//! An archetype is a unique combination of component types. All entities with
//! the same set of components are stored together in the same archetype for
//! cache-friendly iteration.
//!
//! ## Storage Layout
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
//! ## Cache Efficiency
//!
//! When iterating over all entities with Position+Velocity:
//! - SoA (this design): Sequential memory access, excellent cache utilization
//! - AoS alternative: Scattered access, poor cache performance
//!
//! The tradeoff is that accessing all components of a single entity requires
//! multiple array lookups, but this is rare compared to bulk iteration
//! in ECS-style approaches.
//!
//! ## Entity Removal
//!
//! When an entity is removed, we use swap-remove to maintain dense storage:
//! 1. Swap the removed entity with the last entity in each component array
//! 2. Pop the last element (now the removed entity's data)
//! 3. Update the swapped entity's location in the entity_locations map
//!
//! This keeps arrays dense without gaps, maintaining O(1) removal.

use std::alloc::{alloc, dealloc, handle_alloc_error, Layout};
use std::collections::HashMap;
use std::ptr::NonNull;

use trait_type_map::{TraitTypeMap, VecFamily};

use crate::component::{Component, ComponentId, ComponentMask, ComponentTicks};
use crate::entity::Entity;

// =============================================================================
// StorageFactory
// =============================================================================

/// Type for component storage factory functions
/// These create empty storage for a specific component type
pub enum StorageFactory {
    Native(Box<dyn Fn(&mut TraitTypeMap<dyn Component, VecFamily>) + Send + Sync>),
    Dynamic(DynamicComponentLayout),
}

/// Runtime layout for a component whose concrete type is owned by another language.
#[derive(Debug, Clone)]
pub struct DynamicComponentLayout {
    pub size: usize,
    pub align: usize,
    pub schema_hash: u64,
}

/// Aligned, densely packed storage for a type-erased component column.
pub struct DynamicColumn {
    layout: DynamicComponentLayout,
    data: NonNull<u8>,
    len: usize,
    capacity: usize,
}

// SAFETY: Dynamic manifests admit only unmanaged value types. Access remains
// protected by the same scheduler rules as native component columns.
unsafe impl Send for DynamicColumn {}
unsafe impl Sync for DynamicColumn {}

impl DynamicColumn {
    pub fn new(layout: DynamicComponentLayout) -> Self {
        Self {
            layout,
            data: NonNull::dangling(),
            len: 0,
            capacity: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn element_size(&self) -> usize {
        self.layout.size
    }

    pub fn alignment(&self) -> usize {
        self.layout.align
    }

    pub fn schema_hash(&self) -> u64 {
        self.layout.schema_hash
    }

    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.data.as_ptr()
    }

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

    pub fn push_bytes(&mut self, bytes: &[u8]) -> Result<(), &'static str> {
        if bytes.len() != self.layout.size {
            return Err("dynamic component byte length does not match its registered size");
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

    pub fn set_bytes(&mut self, index: usize, bytes: &[u8]) -> Result<(), &'static str> {
        if index >= self.len || bytes.len() != self.layout.size {
            return Err("dynamic component row or byte length is invalid");
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

    fn reserve_one(&mut self) {
        if self.len < self.capacity {
            return;
        }
        let new_capacity = self.capacity.max(4).saturating_mul(2);
        let new_layout = Layout::from_size_align(
            self.layout
                .size
                .checked_mul(new_capacity)
                .expect("dynamic column too large"),
            self.layout.align,
        )
        .expect("invalid dynamic component layout");
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
        self.data = new_data;
        self.capacity = new_capacity;
    }
}

impl Drop for DynamicColumn {
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
pub struct ArchetypeId(pub u128);

// =============================================================================
// Archetype
// =============================================================================

/// Core storage unit grouping entities with the same component set.
///
/// Uses a Structure of Arrays (SoA) layout for cache-friendly bulk iteration.
pub struct Archetype {
    pub id: ArchetypeId,
    pub component_types: Vec<ComponentId>, // Still needed for iteration/lookup
    pub component_mask: ComponentMask,     // Fast bitmask for query matching
    pub component_storages: TraitTypeMap<dyn Component, VecFamily>,
    pub dynamic_component_storages: HashMap<ComponentId, DynamicColumn>,
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
    /// Create a new archetype with storage for the specified component types
    ///
    /// The storage_factories map provides a way to create storage for each component type
    /// by ComponentId (TypeId). This allows archetype creation without knowing the concrete types.
    pub fn new(
        id: ArchetypeId,
        component_types: Vec<ComponentId>,
        component_mask: ComponentMask,
        storage_factories: &HashMap<ComponentId, StorageFactory>,
    ) -> Self {
        let component_count = component_types.len();
        let _zone = crate::profile_scope!(
            "archetype new",
            [("Component types in this archetype: {}", component_count)]
        );
        let mut component_storages = TraitTypeMap::with_capacity(component_count);
        let mut dynamic_component_storages = HashMap::new();
        let mut component_ticks: HashMap<ComponentId, Vec<ComponentTicks>> =
            HashMap::with_capacity(component_count);

        // Register storage for each component type using the factory
        for &component_id in &component_types {
            let factory = storage_factories.get(&component_id)
                .unwrap_or_else(|| panic!(
                    "Component type {:?} not registered. Call world.register_component::<T>() first.",
                    component_id
                ));
            match factory {
                StorageFactory::Native(factory) => factory(&mut component_storages),
                StorageFactory::Dynamic(layout) => {
                    dynamic_component_storages
                        .insert(component_id, DynamicColumn::new(layout.clone()));
                }
            }
            component_ticks.insert(component_id, Vec::new());
        }

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

    /// Check if this archetype contains entities with the specified component
    ///
    /// Uses bitmask for O(1) lookup instead of linear search through component types.
    #[inline]
    pub fn has_component_bit(&self, bit: u8) -> bool {
        self.component_mask.has_bit(bit)
    }

    /// Check if this archetype contains entities with the specified component type
    ///
    /// Note: This uses O(n) linear search. Prefer `has_component_bit` with a
    /// pre-looked-up bit index for hot paths.
    pub fn has_component<T: Component>(&self) -> bool {
        self.component_types.contains(&ComponentId::of::<T>())
    }

    /// Check if this archetype matches the required component mask for a query.
    ///
    /// Called in the query setup hot path for every archetype.
    #[inline]
    pub fn matches_mask(&self, required_mask: &ComponentMask) -> bool {
        self.component_mask.contains_all(required_mask)
    }

    /// Get the number of entities in this archetype.
    #[inline]
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    /// Get the number of entities in this archetype (alias for `len`)
    ///
    /// Provided for API consistency with other collection types.
    #[inline]
    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    /// Check if this archetype contains no entities
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

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

    /// Print information about this archetype (component names and entity count).
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
        // Entity IDs: 16 bytes each
        let mut total = self.entities.capacity() * std::mem::size_of::<crate::entity::Entity>();

        // Component columns: capacity x element size
        for &component_id in &self.component_types {
            if let Some(size) = registry.get_size(&component_id) {
                // We can't inspect the Vec's capacity through the trait object,
                // so we use entity count as a lower bound. The actual Vec capacity
                // may be larger due to pre-allocation.
                total += self.entities.len() * size;
            }
        }

        // Change-detection ticks: 8 bytes each
        total += self.component_types.len() * self.entities.capacity() * 8;

        // HashMap overhead for component_ticks (approximate)
        total += self.component_types.len() * 64;

        total
    }
}
