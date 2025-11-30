// ============================================================================
// Archetype Storage System
// ============================================================================
//! Archetype-based component storage.
//!
//! An archetype is a unique combination of component types. All entities with
//! the same set of components are stored together in the same archetype for
//! cache-friendly iteration.

use trait_type_map::{TraitTypeMap, VecFamily};

use crate::component::{Component, ComponentId, ComponentMask};
use crate::entity::Entity;

/// Type for component storage factory functions
/// These create empty storage for a specific component type
pub type StorageFactory = Box<dyn Fn(&mut TraitTypeMap<dyn Component, VecFamily>)>;

/// ArchetypeId uniquely identifies an archetype (a unique combination of components)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArchetypeId(pub usize);

/// Archetype stores all entities that share the same set of components
///
/// This is the core storage structure for the ECS. Components are stored in
/// a columnar format (Structure of Arrays) using TraitTypeMap for true
/// contiguous memory layout and cache efficiency.
pub struct Archetype {
    pub id: ArchetypeId,
    pub component_types: Vec<ComponentId>, // Still needed for iteration/lookup
    pub component_mask: ComponentMask,     // Fast bitmask for query matching
    pub component_storages: TraitTypeMap<dyn Component, VecFamily>,
    pub entities: Vec<Entity>,
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
        storage_factories: &std::collections::HashMap<ComponentId, StorageFactory>,
    ) -> Self {
        let mut component_storages = TraitTypeMap::new();

        // Register storage for each component type using the factory
        for &comp_id in &component_types {
            if let Some(factory) = storage_factories.get(&comp_id) {
                factory(&mut component_storages);
            } else {
                panic!(
                    "Component type {:?} not registered in storage factories",
                    comp_id
                );
            }
        }

        Self {
            id,
            component_types,
            component_mask,
            component_storages,
            entities: Vec::new(),
        }
    }

    /// Check if this archetype contains entities with the specified component
    pub fn has_component<T: Component>(&self) -> bool {
        self.component_types.contains(&ComponentId::of::<T>())
    }

    /// Check if this archetype matches the required component mask for a query
    pub fn matches_mask(&self, required_mask: &ComponentMask) -> bool {
        self.component_mask.contains_all(required_mask)
    }

    /// Get the number of entities in this archetype
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    /// Print information about this archetype (component names and entity count)
    pub fn print_info(&self, registry: &crate::component::ComponentRegistry) {
        let component_names: Vec<String> = self
            .component_types
            .iter()
            .map(|comp_id| registry.get_name(comp_id).unwrap_or("Unknown").to_string())
            .collect();

        println!(
            "Archetype {:?}: {} entities, components: [{}]",
            self.id,
            self.entities.len(),
            component_names.join(", ")
        );
    }
}
