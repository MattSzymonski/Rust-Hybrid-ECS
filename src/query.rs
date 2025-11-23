// ============================================================================
// Query System - Component Access and Iteration
// ============================================================================
//! Queries provide efficient iteration over entities with specific components.
//!
//! The query system uses the WorldQuery trait to support flexible component
//! access patterns, including mutable and immutable references.

use crate::archetype::{Archetype, ArchetypeId};
use crate::component::{Component, ComponentId, ComponentMask};
use crate::entity::Entity;
use crate::world::World;

/// WorldQuery trait for fetching components from archetypes
///
/// This trait is implemented for different query patterns:
/// - Entity: Access to entity IDs
/// - &T: Immutable component reference
/// - &mut T: Mutable component reference
/// - Tuples: Multiple components at once
pub trait WorldQuery {
    type Item<'a>;

    /// Get the list of component IDs required by this query
    fn component_ids() -> Vec<ComponentId>;

    /// Fetch components from an archetype (immutable)
    fn fetch<'a>(archetype: &'a Archetype, index: usize) -> Self::Item<'a>;

    /// Fetch components from an archetype (mutable)
    fn fetch_mut<'a>(archetype: &'a mut Archetype, index: usize) -> Self::Item<'a>;
}

/// Implement WorldQuery for Entity access
impl WorldQuery for Entity {
    type Item<'a> = Entity;

    fn component_ids() -> Vec<ComponentId> {
        Vec::new()
    }

    fn fetch<'a>(archetype: &'a Archetype, index: usize) -> Self::Item<'a> {
        archetype.entities[index]
    }

    fn fetch_mut<'a>(archetype: &'a mut Archetype, index: usize) -> Self::Item<'a> {
        archetype.entities[index]
    }
}

/// Implement WorldQuery for immutable component reference
impl<T: Component> WorldQuery for &T {
    type Item<'a> = &'a T;

    fn component_ids() -> Vec<ComponentId> {
        vec![ComponentId::of::<T>()]
    }

    fn fetch<'a>(archetype: &'a Archetype, index: usize) -> Self::Item<'a> {
        archetype
            .component_storages
            .get_storage::<T>()
            .get(index)
            .expect("Component not found in archetype")
    }

    fn fetch_mut<'a>(archetype: &'a mut Archetype, index: usize) -> Self::Item<'a> {
        archetype
            .component_storages
            .get_storage_mut::<T>()
            .get_mut(index)
            .expect("Component not found in archetype")
    }
}

/// Implement WorldQuery for mutable component reference
impl<T: Component> WorldQuery for &mut T {
    type Item<'a> = &'a mut T;

    fn component_ids() -> Vec<ComponentId> {
        vec![ComponentId::of::<T>()]
    }

    fn fetch<'a>(_archetype: &'a Archetype, _index: usize) -> Self::Item<'a> {
        panic!("Cannot fetch mutable reference from immutable archetype")
    }

    fn fetch_mut<'a>(archetype: &'a mut Archetype, index: usize) -> Self::Item<'a> {
        archetype
            .component_storages
            .get_storage_mut::<T>()
            .get_mut(index)
            .expect("Component not found in archetype")
    }
}

/// Macro to implement WorldQuery for tuples of different sizes
///
/// This allows queries like Query<(Entity, &Transform, &mut Velocity)>
macro_rules! impl_world_query_tuple {
    ($($T:ident),*) => {
        impl<$($T: WorldQuery),*> WorldQuery for ($($T,)*) {
            type Item<'a> = ($($T::Item<'a>,)*);

            fn component_ids() -> Vec<ComponentId> {
                let mut ids = Vec::new();
                $(ids.extend($T::component_ids());)*
                ids
            }

            #[allow(non_snake_case)]
            fn fetch<'a>(archetype: &'a Archetype, index: usize) -> Self::Item<'a> {
                ($($T::fetch(archetype, index),)*)
            }

            #[allow(non_snake_case)]
            fn fetch_mut<'a>(archetype: &'a mut Archetype, index: usize) -> Self::Item<'a> {
                // SAFETY: We use raw pointers to allow multiple mutable borrows of different components
                let arch_ptr = archetype as *mut Archetype;
                unsafe {
                    ($($T::fetch_mut(&mut *arch_ptr, index),)*)
                }
            }
        }
    };
}

// Implement for tuples up to 4 elements
impl_world_query_tuple!(A);
impl_world_query_tuple!(A, B);
impl_world_query_tuple!(A, B, C);
impl_world_query_tuple!(A, B, C, D);

/// Query provides iteration over entities matching a component pattern
///
/// Example:
/// ```ignore
/// fn my_system(mut query: Query<(Entity, &Transform, &mut Velocity)>) {
///     for (entity, transform, velocity) in query.iter_mut() {
///         // Process entities with Transform and Velocity components
///     }
/// }
/// ```
pub struct Query<'w, Q: WorldQuery> {
    world: &'w mut World,
    _phantom: std::marker::PhantomData<Q>,
}

impl<'w, Q: WorldQuery> Query<'w, Q> {
    pub fn new(world: &'w mut World) -> Self {
        Self {
            world,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Create an iterator over all matching entities
    pub fn iter_mut(&mut self) -> QueryIterMut<Q> {
        // Build component mask from query requirements
        let component_ids = Q::component_ids();
        let mut query_mask = ComponentMask::empty();
        for comp_id in &component_ids {
            if let Some(bit) = crate::component::get_component_bit_by_id(comp_id) {
                query_mask.set(bit);
            }
        }

        let matching_archetypes: Vec<ArchetypeId> = self
            .world
            .archetypes
            .iter()
            .filter(|(_, archetype)| archetype.matches_mask(&query_mask))
            .map(|(id, _)| *id)
            .collect();

        QueryIterMut {
            world_ptr: self.world as *mut World,
            matching_archetypes,
            current_archetype_idx: 0,
            current_entity_idx: 0,
            current_archetype_ptr: std::ptr::null_mut(),
            current_archetype_len: 0,
            _phantom: std::marker::PhantomData,
        }
    }
}

/// Iterator for mutable queries
///
/// This iterator walks through all archetypes that match the query pattern
/// and yields components for each entity.
pub struct QueryIterMut<'w, Q: WorldQuery> {
    world_ptr: *mut World,
    matching_archetypes: Vec<ArchetypeId>,
    current_archetype_idx: usize,
    current_entity_idx: usize,
    // Cache the current archetype pointer and length to avoid repeated lookups
    current_archetype_ptr: *mut Archetype,
    current_archetype_len: usize,
    _phantom: std::marker::PhantomData<&'w mut Q>,
}

impl<'w, Q: WorldQuery> Iterator for QueryIterMut<'w, Q> {
    type Item = Q::Item<'w>;

    fn next(&mut self) -> Option<Self::Item> {
        unsafe {
            loop {
                // Fast path: iterate within current archetype
                if self.current_entity_idx < self.current_archetype_len {
                    let index = self.current_entity_idx;
                    self.current_entity_idx += 1;

                    // SAFETY: Lifetime extension is safe because:
                    // 1. We hold exclusive access to world through world_ptr
                    // 2. Each iteration yields unique component references
                    // 3. References don't outlive the Query itself
                    return Some(Q::fetch_mut(&mut *self.current_archetype_ptr, index));
                }

                // Move to next archetype
                if self.current_archetype_idx >= self.matching_archetypes.len() {
                    return None;
                }

                let world = &mut *self.world_ptr;
                let archetype_id = self.matching_archetypes[self.current_archetype_idx];
                let archetype = world.archetypes.get_mut(&archetype_id)?;

                // Cache archetype pointer and length
                self.current_archetype_ptr = archetype as *mut Archetype;
                self.current_archetype_len = archetype.len();
                self.current_entity_idx = 0;
                self.current_archetype_idx += 1;

                // Continue to next iteration (will hit fast path if archetype has entities)
            }
        }
    }
}

/// Query for accessing global (singleton) components
///
/// Unlike regular Query which iterates over entities, GlobalComponentQuery
/// provides access to singleton components stored directly in the World.
///
/// Example:
/// ```ignore
/// fn my_system(time: GlobalComponentQuery<GlobalTime>) {
///     if let Some(time) = time.get() {
///         println!("Delta time: {}", time.delta_time);
///     }
/// }
/// ```
pub struct GlobalComponentQuery<'w, T: Component> {
    world: &'w mut World,
    _phantom: std::marker::PhantomData<T>,
}

impl<'w, T: Component> GlobalComponentQuery<'w, T> {
    pub fn new(world: &'w mut World) -> Self {
        Self {
            world,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Get immutable reference to the global component
    pub fn get(&self) -> Option<&T> {
        self.world.get_global_component::<T>()
    }

    /// Get mutable reference to the global component
    pub fn get_mut(&mut self) -> Option<&mut T> {
        self.world.get_global_component_mut::<T>()
    }
}
