// ============================================================================
// Archetype Storage System
// ============================================================================
//! Archetype-based component storage.
//!
//! An archetype is a unique combination of component types. All entities with
//! the same set of components are stored together in the same archetype for
//! cache-friendly iteration.

use std::any::Any;
use std::collections::HashMap;

use crate::component::{Component, ComponentId};
use crate::entity::Entity;

/// Stores components in columns for cache-friendly iteration
///
/// Each column stores all instances of a single component type for all
/// entities in an archetype. This layout is efficient for iteration.
pub(crate) struct ComponentColumn {
    pub data: Vec<Box<dyn Any>>,
    pub component_id: ComponentId,
}

impl ComponentColumn {
    pub fn new(component_id: ComponentId) -> Self {
        Self {
            data: Vec::new(),
            component_id,
        }
    }

    pub fn get<T: Component>(&self, index: usize) -> Option<&T> {
        self.data.get(index)?.downcast_ref::<T>()
    }

    pub fn get_mut<T: Component>(&mut self, index: usize) -> Option<&mut T> {
        self.data.get_mut(index)?.downcast_mut::<T>()
    }
}

/// ArchetypeId uniquely identifies an archetype (a unique combination of components)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ArchetypeId(pub usize);

/// Archetype stores all entities that share the same set of components
///
/// This is the core storage structure for the ECS. Components are stored in
/// a columnar format (Structure of Arrays) for cache efficiency.
pub(crate) struct Archetype {
    pub id: ArchetypeId,
    pub component_types: Vec<ComponentId>,
    pub columns: HashMap<ComponentId, ComponentColumn>,
    pub entities: Vec<Entity>,
}

impl Archetype {
    pub fn new(id: ArchetypeId, component_types: Vec<ComponentId>) -> Self {
        let mut columns = HashMap::new();
        for &comp_id in &component_types {
            columns.insert(comp_id, ComponentColumn::new(comp_id));
        }

        Self {
            id,
            component_types,
            columns,
            entities: Vec::new(),
        }
    }

    /// Check if this archetype contains entities with the specified component
    pub fn has_component<T: Component>(&self) -> bool {
        self.component_types.contains(&ComponentId::of::<T>())
    }

    /// Check if this archetype matches the required component set for a query
    pub fn matches_components(&self, component_ids: &[ComponentId]) -> bool {
        component_ids
            .iter()
            .all(|id| self.component_types.contains(id))
    }

    /// Get the number of entities in this archetype
    pub fn len(&self) -> usize {
        self.entities.len()
    }
}
