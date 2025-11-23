// ============================================================================
// World - Central ECS State Management
// ============================================================================
//! The World is the central container for all ECS data.
//!
//! It manages entities, archetypes, and provides the primary interface for
//! spawning entities and managing components.

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use trait_type_map::{TraitAccessible, TraitTypeMap, VecFamily};

use crate::archetype::{Archetype, ArchetypeId, StorageFactory};
use crate::component::{register_component_bit, Component, ComponentId, ComponentMask};
use crate::entity::Entity;

/// Function that copies a component from one storage to another at given indices
type ComponentCopier = Arc<
    dyn Fn(
            &TraitTypeMap<dyn Component, VecFamily>,
            &mut TraitTypeMap<dyn Component, VecFamily>,
            usize,
        ) + Send
        + Sync,
>;

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
/// - Maintains component type registry for creating archetype storage
pub struct World {
    next_free_entity_id: u64,
    pub(crate) archetypes: HashMap<ArchetypeId, Archetype>,
    next_free_archetype_id: usize,
    pub(crate) entity_locations: HashMap<Entity, EntityLocation>,
    archetype_lookup: HashMap<ComponentMask, ArchetypeId>, // Changed from Vec<ComponentId> to ComponentMask
    pub(crate) global_components: HashMap<ComponentId, Box<dyn Any>>,
    /// Storage factories for creating component storage by TypeId
    storage_factories: HashMap<ComponentId, StorageFactory>,
    /// Component copiers for moving entities between archetypes
    pub(crate) component_copiers: HashMap<ComponentId, ComponentCopier>,
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
            storage_factories: HashMap::new(),
            component_copiers: HashMap::new(),
        }
    }

    /// Register a component type with the World
    ///
    /// This must be called for each component type before it can be used.
    /// The registration creates a factory function that can create storage
    /// for this component type without needing the generic type parameter.
    /// Also registers the component bit in the global registry.
    pub fn register_component<T>(&mut self)
    where
        T: Component + TraitAccessible<dyn Component> + Clone,
    {
        let comp_id = ComponentId::of::<T>();

        // Register bit index in global registry
        register_component_bit::<T>();

        self.storage_factories.insert(
            comp_id,
            Box::new(|map: &mut TraitTypeMap<dyn Component, VecFamily>| {
                map.register_type_storage::<T>();
            }),
        );

        // Register copier function for this component type
        self.component_copiers.insert(
            comp_id,
            Arc::new(
                |src: &TraitTypeMap<dyn Component, VecFamily>,
                 dst: &mut TraitTypeMap<dyn Component, VecFamily>,
                 index: usize| {
                    if let Some(component) = src.get_storage::<T>().get(index) {
                        dst.get_storage_mut::<T>().push(component.clone());
                    }
                },
            ),
        );
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

        // Build component mask from component IDs
        let mut component_mask = ComponentMask::empty();
        for comp_id in &component_ids {
            if let Some(bit) = crate::component::get_component_bit_by_id(comp_id) {
                component_mask.set(bit);
            }
        }

        if let Some(&archetype_id) = self.archetype_lookup.get(&component_mask) {
            return archetype_id;
        }

        let new_archetype_id = ArchetypeId(self.next_free_archetype_id);
        self.next_free_archetype_id += 1;

        // Create archetype with storage for all component types
        let new_archetype = Archetype::new(
            new_archetype_id,
            component_ids.clone(),
            component_mask,
            &self.storage_factories,
        );
        self.archetypes.insert(new_archetype_id, new_archetype);
        self.archetype_lookup
            .insert(component_mask, new_archetype_id);

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
        let archetype_id = self.get_or_create_archetype(component_ids);

        let archetype = self.archetypes.get_mut(&archetype_id).unwrap();
        let index = archetype.entities.len();

        // Add entity to archetype
        archetype.entities.push(entity);

        // Use the provided closure to insert components with their concrete types
        insert_fn(&mut archetype.component_storages);

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
    pub(crate) fn move_entity_to_archetype<F>(
        &mut self,
        entity: Entity,
        new_component_ids: Vec<ComponentId>,
        move_fn: F,
    ) where
        F: FnOnce(
            &TraitTypeMap<dyn Component, VecFamily>,
            &mut TraitTypeMap<dyn Component, VecFamily>,
            usize,
        ),
    {
        // Get current location
        let old_location = match self.entity_locations.get(&entity) {
            Some(loc) => *loc,
            None => {
                println!("  [Warning] Entity {:?} not found in world", entity.id);
                return;
            }
        };

        let old_archetype_id = old_location.archetype_id;
        let old_index = old_location.index_in_archetype;

        // Get or create new archetype
        let new_archetype_id = self.get_or_create_archetype(new_component_ids);

        // If same archetype, nothing to do (shouldn't happen for add_component)
        if old_archetype_id == new_archetype_id {
            println!(
                "  [Warning] Entity {:?} already has this component",
                entity.id
            );
            return;
        }

        // We need to:
        // 1. Copy components from old to new archetype
        // 2. Remove entity from old archetype
        // 3. Add entity to new archetype

        // SAFETY: We need to access two archetypes simultaneously
        // We ensure old_archetype_id != new_archetype_id above
        let old_arch_ptr = self.archetypes.get(&old_archetype_id).unwrap() as *const Archetype;
        let new_arch_ptr = self.archetypes.get_mut(&new_archetype_id).unwrap() as *mut Archetype;

        unsafe {
            let old_arch = &*old_arch_ptr;
            let new_arch = &mut *new_arch_ptr;

            let new_index = new_arch.entities.len();
            new_arch.entities.push(entity);

            // Call the move function to copy components
            move_fn(
                &old_arch.component_storages,
                &mut new_arch.component_storages,
                old_index,
            );

            // Update entity location
            self.entity_locations.insert(
                entity,
                EntityLocation {
                    archetype_id: new_archetype_id,
                    index_in_archetype: new_index,
                },
            );
        }

        // Remove entity from old archetype
        // We can't easily swap_remove without also swapping component data,
        // so we use regular remove (O(n) but simpler for now)
        let old_archetype = self.archetypes.get_mut(&old_archetype_id).unwrap();
        old_archetype.entities.remove(old_index);

        // Update indices for all entities after this one
        for i in old_index..old_archetype.entities.len() {
            let ent = old_archetype.entities[i];
            self.entity_locations
                .get_mut(&ent)
                .unwrap()
                .index_in_archetype = i;
        }

        // Note: Component data remains in storage. A production implementation would
        // also remove components, but TraitTypeMap's VecOptionStorage makes this complex.

        // Clean up empty archetype
        if old_archetype.entities.is_empty() {
            // Don't remove archetype - it might be reused
            // Removing would require updating archetype_lookup which is complex
        }
    }
}

/// Trait for inserting a component into storage
trait ComponentInserter {
    fn insert(self: Box<Self>, storage: &mut TraitTypeMap<dyn Component, VecFamily>);
    fn component_id(&self) -> ComponentId;
}

/// Implementation that captures the concrete component type
struct TypedComponentInserter<T: Component + TraitAccessible<dyn Component>> {
    component: T,
}

impl<T: Component + TraitAccessible<dyn Component>> ComponentInserter
    for TypedComponentInserter<T>
{
    fn insert(self: Box<Self>, storage: &mut TraitTypeMap<dyn Component, VecFamily>) {
        storage.get_storage_mut::<T>().push(self.component);
    }

    fn component_id(&self) -> ComponentId {
        ComponentId::of::<T>()
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

    /// Finish building and insert the entity into the world
    pub fn build(self) -> Entity {
        let entity = self.entity;
        let component_ids: Vec<ComponentId> =
            self.components.iter().map(|c| c.component_id()).collect();

        let components = self.components;
        self.world
            .insert_entity_with_components(entity, component_ids, |storage| {
                for inserter in components {
                    inserter.insert(storage);
                }
            });
        entity
    }
}

// How i am supposed to create new archetype if I need generic types to do so (register component storages), and I have only type ids?
