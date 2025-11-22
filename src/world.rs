// ============================================================================
// World - Central ECS State Management
// ============================================================================
//! The World is the central container for all ECS data.
//!
//! It manages entities, archetypes, and provides the primary interface for
//! spawning entities and managing components.

use std::any::Any;
use std::collections::HashMap;

use crate::archetype::{Archetype, ArchetypeId};
use crate::component::{Component, ComponentId};
use crate::entity::Entity;

/// EntityLocation tracks where an entity is stored in the archetype system
#[derive(Clone, Copy)]
pub(crate) struct EntityLocation {
    pub archetype_id: ArchetypeId,
    pub index_in_archetype: usize,
}

/// World manages all entities, archetypes, and global components
///
/// This is the central hub of the ECS. It:
/// - Allocates entity IDs
/// - Manages archetype storage
/// - Tracks entity locations
/// - Stores global (singleton) components
pub struct World {
    next_free_entity_id: u64,
    pub(crate) archetypes: HashMap<ArchetypeId, Archetype>,
    next_free_archetype_id: usize,
    pub(crate) entity_locations: HashMap<Entity, EntityLocation>,
    archetype_lookup: HashMap<Vec<ComponentId>, ArchetypeId>,
    pub(crate) global_components: HashMap<ComponentId, Box<dyn Any>>,
}

impl World {
    /// Create a new empty World
    pub fn new() -> Self {
        Self {
            next_free_entity_id: 0,
            archetypes: HashMap::new(),
            next_free_archetype_id: 0,
            entity_locations: HashMap::new(),
            archetype_lookup: HashMap::new(),
            global_components: HashMap::new(),
        }
    }

    /// Add or update a global component (singleton not attached to any entity)
    ///
    /// Global components are useful for singleton data like time, input state,
    /// or game configuration that doesn't belong to any specific entity.
    pub fn add_global_component<T: Component>(&mut self, component: T) {
        self.global_components
            .insert(ComponentId::of::<T>(), Box::new(component));
    }

    /// Get immutable reference to a global component
    pub fn get_global_component<T: Component>(&self) -> Option<&T> {
        self.global_components
            .get(&ComponentId::of::<T>())
            .and_then(|boxed| boxed.downcast_ref::<T>())
    }

    /// Get mutable reference to a global component
    pub fn get_global_component_mut<T: Component>(&mut self) -> Option<&mut T> {
        self.global_components
            .get_mut(&ComponentId::of::<T>())
            .and_then(|boxed| boxed.downcast_mut::<T>())
    }

    /// Allocate a new unique entity ID
    pub(crate) fn allocate_entity(&mut self) -> Entity {
        let entity = Entity {
            id: self.next_free_entity_id,
            generation: 0,
        };
        self.next_free_entity_id += 1;
        entity
    }

    /// Get or create an archetype for a given set of components
    ///
    /// Archetypes are cached and reused for entities with the same component set.
    pub(crate) fn get_or_create_archetype(
        &mut self,
        mut component_ids: Vec<ComponentId>,
    ) -> ArchetypeId {
        component_ids.sort();

        if let Some(&archetype_id) = self.archetype_lookup.get(&component_ids) {
            return archetype_id;
        }

        let new_archetype_id = ArchetypeId(self.next_free_archetype_id);
        self.next_free_archetype_id += 1;

        let new_archetype = Archetype::new(new_archetype_id, component_ids.clone());
        self.archetypes.insert(new_archetype_id, new_archetype);
        self.archetype_lookup
            .insert(component_ids, new_archetype_id);

        new_archetype_id
    }

    /// Start building a new entity
    ///
    /// Returns an EntityBuilder that allows fluent API for adding components.
    pub fn spawn(&mut self) -> EntityBuilder {
        let entity = self.allocate_entity();
        EntityBuilder {
            world: self,
            entity,
            components: Vec::new(),
        }
    }

    /// Insert an entity with its components into the appropriate archetype
    pub(crate) fn insert_entity_with_components(
        &mut self,
        entity: Entity,
        components: Vec<(ComponentId, Box<dyn Any>)>,
    ) {
        let component_ids: Vec<ComponentId> = components.iter().map(|(id, _)| *id).collect();
        let archetype_id = self.get_or_create_archetype(component_ids);

        let archetype = self.archetypes.get_mut(&archetype_id).unwrap();
        let index = archetype.entities.len();

        // Add entity to archetype and its components to the appropriate archetype columns
        archetype.entities.push(entity);
        for (component_id, component) in components {
            if let Some(column) = archetype.columns.get_mut(&component_id) {
                column.data.push(component);
            }
        }

        self.entity_locations.insert(
            entity,
            EntityLocation {
                archetype_id,
                index_in_archetype: index,
            },
        );
    }
}

/// Builder for constructing entities with components using a fluent API
///
/// Example:
/// ```ignore
/// world.spawn()
///     .with(Transform { x: 0.0, y: 0.0, z: 0.0 })
///     .with(Velocity { x: 10.0 })
///     .build();
/// ```
pub struct EntityBuilder<'w> {
    world: &'w mut World,
    entity: Entity,
    components: Vec<(ComponentId, Box<dyn Any>)>,
}

impl<'w> EntityBuilder<'w> {
    /// Add a component to the entity being built
    pub fn with<T: Component>(mut self, component: T) -> Self {
        self.components
            .push((ComponentId::of::<T>(), Box::new(component)));
        self
    }

    /// Finish building and insert the entity into the world
    pub fn build(self) -> Entity {
        let entity = self.entity;
        self.world
            .insert_entity_with_components(entity, self.components);
        entity
    }
}
