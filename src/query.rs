// ============================================================================
// Query System - Component Access and Iteration
// ============================================================================
//! Queries provide efficient iteration over entities with specific components.
//!
//! The query system uses the QueryTarget trait to support flexible component
//! access patterns, including mutable and immutable references.

use crate::archetype::{Archetype, ArchetypeId};
use crate::component::{Component, ComponentId, ComponentMask};
use crate::entity::Entity;
use crate::world::World;

use trait_type_map::VecOptionStorage;

/// QueryTarget trait for fetching components from archetypes
///
/// This trait is implemented for different query patterns:
/// - Entity: Access to entity IDs
/// - &T: Immutable component reference
/// - &mut T: Mutable component reference
/// - Tuples: Multiple components at once
pub trait QueryTarget {
    type Item<'a>;
    type State;

    /// Get the list of component IDs required by this query
    fn component_ids() -> Vec<ComponentId>;

    /// Report component access for dependency analysis
    /// Returns (reads, writes) as vectors of ComponentIds
    fn report_component_access() -> (Vec<ComponentId>, Vec<ComponentId>);

    /// Initialize state for fetching from an archetype (caches storage pointers)
    fn init_state(archetype: &mut Archetype) -> Self::State;

    /// Fetch components using cached state
    fn fetch_mut_with_state<'a>(state: &Self::State, index: usize) -> Self::Item<'a>;

    /// Fetch components from an archetype (immutable)
    fn fetch<'a>(archetype: &'a Archetype, index: usize) -> Self::Item<'a>;

    /// Fetch components from an archetype (mutable)
    fn fetch_mut<'a>(archetype: &'a mut Archetype, index: usize) -> Self::Item<'a>;
}

/// Implement QueryTarget for Entity access
impl QueryTarget for Entity {
    type Item<'a> = Entity;
    type State = *const Vec<Entity>;

    fn component_ids() -> Vec<ComponentId> {
        Vec::new()
    }

    fn report_component_access() -> (Vec<ComponentId>, Vec<ComponentId>) {
        (Vec::new(), Vec::new()) // Entity access doesn't read or write components
    }

    fn init_state(archetype: &mut Archetype) -> Self::State {
        &archetype.entities as *const Vec<Entity>
    }

    fn fetch_mut_with_state<'a>(state: &Self::State, index: usize) -> Self::Item<'a> {
        unsafe {
            let vec_ptr = *state;
            let vec_ref = &*vec_ptr;
            vec_ref.get_unchecked(index).clone()
        }
    }

    fn fetch<'a>(archetype: &'a Archetype, index: usize) -> Self::Item<'a> {
        archetype.entities[index]
    }

    fn fetch_mut<'a>(archetype: &'a mut Archetype, index: usize) -> Self::Item<'a> {
        archetype.entities[index]
    }
}

/// Implement QueryTarget for immutable component reference
impl<T: Component> QueryTarget for &T {
    type Item<'a> = &'a T;
    type State = *const VecOptionStorage<T, dyn Component>;

    fn component_ids() -> Vec<ComponentId> {
        vec![ComponentId::of::<T>()]
    }

    fn report_component_access() -> (Vec<ComponentId>, Vec<ComponentId>) {
        // Immutable reference = read access
        (vec![ComponentId::of::<T>()], Vec::new())
    }

    fn init_state(archetype: &mut Archetype) -> Self::State {
        archetype.component_storages.get_storage::<T>() as *const VecOptionStorage<T, dyn Component>
    }

    fn fetch_mut_with_state<'a>(state: &Self::State, index: usize) -> Self::Item<'a> {
        unsafe { (*(*state)).get(index).expect("Component not found") }
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

/// Implement QueryTarget for mutable component reference
impl<T: Component> QueryTarget for &mut T {
    type Item<'a> = &'a mut T;
    type State = *mut VecOptionStorage<T, dyn Component>;

    fn component_ids() -> Vec<ComponentId> {
        vec![ComponentId::of::<T>()]
    }

    fn report_component_access() -> (Vec<ComponentId>, Vec<ComponentId>) {
        // Mutable reference = write access
        (Vec::new(), vec![ComponentId::of::<T>()])
    }

    fn init_state(archetype: &mut Archetype) -> Self::State {
        archetype.component_storages.get_storage_mut::<T>()
            as *mut VecOptionStorage<T, dyn Component>
    }

    fn fetch_mut_with_state<'a>(state: &Self::State, index: usize) -> Self::Item<'a> {
        unsafe { (*(*state)).get_mut(index).expect("Component not found") }
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

/// Macro to implement QueryTarget for tuples of different sizes
///
/// This allows queries like Query<(Entity, &Transform, &mut Velocity)>
macro_rules! impl_query_object_tuple {
    ($($T:ident),*) => {
        impl<$($T: QueryTarget),*> QueryTarget for ($($T,)*) {
            type Item<'a> = ($($T::Item<'a>,)*);
            type State = ($($T::State,)*);

            fn component_ids() -> Vec<ComponentId> {
                let mut ids = Vec::new();
                $(ids.extend($T::component_ids());)*
                ids
            }

            fn report_component_access() -> (Vec<ComponentId>, Vec<ComponentId>) {
                let mut reads = Vec::new();
                let mut writes = Vec::new();
                $(
                    let (r, w) = $T::report_component_access();
                    reads.extend(r);
                    writes.extend(w);
                )*
                (reads, writes)
            }

            #[allow(non_snake_case)]
            fn init_state(archetype: &mut Archetype) -> Self::State {
                // Get raw pointer to allow multiple init_state calls
                let arch_ptr = archetype as *mut Archetype;
                unsafe {
                    ($($T::init_state(&mut *arch_ptr),)*)
                }
            }

            #[allow(non_snake_case)]
            fn fetch_mut_with_state<'a>(state: &Self::State, index: usize) -> Self::Item<'a> {
                let ($($T,)*) = state;
                ($($T::fetch_mut_with_state($T, index),)*)
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
impl_query_object_tuple!(A);
impl_query_object_tuple!(A, B);
impl_query_object_tuple!(A, B, C);
impl_query_object_tuple!(A, B, C, D);

/// Actual query provides iteration over entities matching a component pattern
///
/// Example:
/// ```ignore
/// fn my_system(mut query: Query<(Entity, &Transform, &mut Velocity)>) {
///     for (entity, transform, velocity) in query.iter_mut() {
///         // Process entities with Transform and Velocity components
///     }
/// }
/// ```
pub struct Query<'w, Q: QueryTarget> {
    world: &'w mut World,
    _phantom: std::marker::PhantomData<Q>,
}

impl<'w, Q: QueryTarget> Query<'w, Q> {
    pub fn new(world: &'w mut World) -> Self {
        Self {
            world,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Create an iterator over all matching entities
    #[inline]
    pub fn iter_mut(&'_ mut self) -> QueryIterMut<'_, Q> {
        // Build component mask from query requirements
        let component_ids = Q::component_ids();
        let mut query_mask = ComponentMask::empty();
        for component_id in &component_ids {
            if let Some(bit) = self.world.component_registry.get_bit(component_id) {
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
            current_archetype_len: 0,
            current_state: None,
            _phantom: std::marker::PhantomData,
        }
    }
}

/// Iterator for mutable queries
///
/// This iterator walks through all archetypes that match the query pattern
/// and yields components for each entity.
pub struct QueryIterMut<'w, Q: QueryTarget> {
    world_ptr: *mut World,
    matching_archetypes: Vec<ArchetypeId>,
    current_archetype_idx: usize,
    current_entity_idx: usize,
    // Cache the current archetype length to avoid repeated lookups
    current_archetype_len: usize,
    // Cache component storage pointers (always Some during iteration)
    current_state: Option<Q::State>,
    _phantom: std::marker::PhantomData<&'w mut Q>,
}

impl<'w, Q: QueryTarget> Iterator for QueryIterMut<'w, Q> {
    type Item = Q::Item<'w>;

    fn next(&mut self) -> Option<Self::Item> {
        unsafe {
            loop {
                // Fast path: iterate within current archetype using cached state
                // This is the hot path that gets executed millions of times
                if self.current_entity_idx < self.current_archetype_len {
                    let index = self.current_entity_idx;
                    self.current_entity_idx += 1;

                    // SAFETY: current_state is always Some during iteration in the fast path
                    // We use unwrap_unchecked to eliminate branch misprediction overhead
                    // The Option check would add a testb+jne branch on every iteration
                    let state = self.current_state.as_ref().unwrap_unchecked();
                    return Some(Q::fetch_mut_with_state(state, index));
                }

                // Cold path: move to next archetype
                // This happens infrequently (once per archetype)
                // Moving to separate function gives 40% boost in overall iteration speed
                self.advance_archetype()?;
            }
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        // Provide size hint for better iterator optimizations
        let remaining = self
            .matching_archetypes
            .get(self.current_archetype_idx..)
            .map(|archs| archs.len())
            .unwrap_or(0);
        (0, Some(remaining * 64)) // Rough estimate: 64 entities per archetype average
    }
}

impl<'w, Q: QueryTarget> QueryIterMut<'w, Q> {
    /// Advance to the next archetype (cold path, separated for better branch prediction)
    #[inline(never)]
    fn advance_archetype(&mut self) -> Option<()> {
        // SAFETY: This function is safe because:
        // 1. world_ptr was created from a valid &mut World reference in iter_mut()
        // 2. The QueryIterMut holds exclusive access to World through its lifetime 'w
        // 3. We never yield references that outlive the iterator itself
        // 4. Each archetype_id comes from matching_archetypes which was populated from valid archetypes
        // 5. The HashMap lookup can fail (returning None) but that's handled by the ? operator
        // 6. init_state() caches raw pointers to component storage, which remain valid because:
        //    - We hold exclusive access to World
        //    - Archetypes are not moved/reallocated during iteration
        //    - Component storage vectors maintain stable addresses while we iterate
        unsafe {
            // Check if we've exhausted all archetypes
            if self.current_archetype_idx >= self.matching_archetypes.len() {
                return None;
            }

            let world = &mut *self.world_ptr;
            let archetype_id = self.matching_archetypes[self.current_archetype_idx];
            let archetype = world.archetypes.get_mut(&archetype_id)?;

            // Cache archetype length and component storage pointers
            self.current_archetype_len = archetype.len();
            self.current_state = Some(Q::init_state(archetype));
            self.current_entity_idx = 0;
            self.current_archetype_idx += 1;

            Some(())
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
