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

use rayon::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use trait_type_map::VecOptionStorage;

// ============================================================================
// Batch Statistics
// ============================================================================

/// Statistics about batch distribution during parallel iteration
///
/// This struct is returned by `for_each_tracked` and `for_each_batched_tracked`
/// methods to provide insight into how Rayon distributed work across threads.
#[derive(Debug, Clone)]
pub struct BatchStats {
    /// Number of threads in the Rayon thread pool
    pub num_threads: usize,
    /// Total number of batches that were executed
    pub batch_count: usize,
    /// Total number of entities that were processed
    pub total_entities: usize,
    /// Size of the smallest batch
    pub min_batch_size: usize,
    /// Size of the largest batch
    pub max_batch_size: usize,
    /// Average batch size (total_entities / batch_count)
    pub avg_batch_size: f64,
}

impl std::fmt::Display for BatchStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "BatchStats {{ threads: {}, batches: {}, entities: {}, min: {}, max: {}, avg: {:.1} }}",
            self.num_threads,
            self.batch_count,
            self.total_entities,
            self.min_batch_size,
            self.max_batch_size,
            self.avg_batch_size
        )
    }
}

// ============================================================================
// Thread-Safe Pointer Wrappers
// ============================================================================

/// A wrapper for `*const T` that implements Send and Sync
///
/// # Safety
/// This is safe because we guarantee that:
/// 1. The pointer points to valid data for the lifetime of the query
/// 2. Different threads access different indices (no aliasing)
/// 3. The World has exclusive access during iteration
#[derive(Clone, Copy)]
pub struct SendPtr<T>(*const T);

unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}

impl<T> SendPtr<T> {
    pub fn new(ptr: *const T) -> Self {
        Self(ptr)
    }

    pub fn as_ptr(&self) -> *const T {
        self.0
    }
}

/// A wrapper for `*mut T` that implements Send and Sync
///
/// # Safety
/// This is safe because we guarantee that:
/// 1. The pointer points to valid data for the lifetime of the query
/// 2. Different threads access different indices (no aliasing)
/// 3. The World has exclusive access during iteration
#[derive(Clone, Copy)]
pub struct SendPtrMut<T>(*mut T);

unsafe impl<T> Send for SendPtrMut<T> {}
unsafe impl<T> Sync for SendPtrMut<T> {}

impl<T> SendPtrMut<T> {
    pub fn new(ptr: *mut T) -> Self {
        Self(ptr)
    }

    pub fn as_ptr(&self) -> *mut T {
        self.0
    }
}

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
    type State = SendPtr<Vec<Entity>>;

    fn component_ids() -> Vec<ComponentId> {
        Vec::new()
    }

    fn report_component_access() -> (Vec<ComponentId>, Vec<ComponentId>) {
        (Vec::new(), Vec::new()) // Entity access doesn't read or write components
    }

    fn init_state(archetype: &mut Archetype) -> Self::State {
        SendPtr::new(&archetype.entities as *const Vec<Entity>)
    }

    fn fetch_mut_with_state<'a>(state: &Self::State, index: usize) -> Self::Item<'a> {
        unsafe {
            let vec_ref = &*state.as_ptr();
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
    type State = SendPtr<VecOptionStorage<T, dyn Component>>;

    fn component_ids() -> Vec<ComponentId> {
        vec![ComponentId::of::<T>()]
    }

    fn report_component_access() -> (Vec<ComponentId>, Vec<ComponentId>) {
        // Immutable reference = read access
        (vec![ComponentId::of::<T>()], Vec::new())
    }

    fn init_state(archetype: &mut Archetype) -> Self::State {
        SendPtr::new(archetype.component_storages.get_storage::<T>()
            as *const VecOptionStorage<T, dyn Component>)
    }

    fn fetch_mut_with_state<'a>(state: &Self::State, index: usize) -> Self::Item<'a> {
        unsafe { (*state.as_ptr()).get(index).expect("Component not found") }
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
    type State = SendPtrMut<VecOptionStorage<T, dyn Component>>;

    fn component_ids() -> Vec<ComponentId> {
        vec![ComponentId::of::<T>()]
    }

    fn report_component_access() -> (Vec<ComponentId>, Vec<ComponentId>) {
        // Mutable reference = write access
        (Vec::new(), vec![ComponentId::of::<T>()])
    }

    fn init_state(archetype: &mut Archetype) -> Self::State {
        SendPtrMut::new(archetype.component_storages.get_storage_mut::<T>()
            as *mut VecOptionStorage<T, dyn Component>)
    }

    fn fetch_mut_with_state<'a>(state: &Self::State, index: usize) -> Self::Item<'a> {
        unsafe {
            (*state.as_ptr())
                .get_mut(index)
                .expect("Component not found")
        }
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

    /// Get the first matching entity's components, if any exist
    ///
    /// This is useful when you expect only one entity or just need any matching entity.
    ///
    /// # Example
    /// ```ignore
    /// if let Some((transform, velocity)) = query.first() {
    ///     println!("First entity at ({}, {})", transform.x, transform.y);
    /// }
    /// ```
    #[inline]
    pub fn first(&mut self) -> Option<Q::Item<'_>> {
        self.iter_mut().next()
    }

    /// Create a parallel iterator over all matching entities using Rayon
    ///
    /// This method provides parallel iteration across entities, distributing
    /// work across multiple threads. Each archetype is processed in parallel,
    /// and entities within each archetype are also processed in parallel.
    ///
    /// # Example
    /// ```ignore
    /// fn physics_system(mut query: Query<(&mut Transform, &Velocity)>) {
    ///     query.par_iter_mut().for_each(|(transform, velocity)| {
    ///         transform.x += velocity.x * 0.016;
    ///         transform.y += velocity.y * 0.016;
    ///     });
    /// }
    /// ```
    ///
    /// # Safety
    /// This is safe because:
    /// - Each entity's components are accessed by exactly one thread
    /// - Different entities have independent component data
    /// - The query holds exclusive access to the World
    #[inline]
    pub fn par_iter_mut(&'_ mut self) -> ParQueryIterMut<'_, Q>
    where
        Q::State: Send + Sync,
        for<'a> Q::Item<'a>: Send,
    {
        // Build component mask from query requirements
        let component_ids = Q::component_ids();
        let mut query_mask = ComponentMask::empty();
        for component_id in &component_ids {
            if let Some(bit) = self.world.component_registry.get_bit(component_id) {
                query_mask.set(bit);
            }
        }

        // Collect matching archetypes with their entity ranges
        let mut archetype_ranges: Vec<(ArchetypeId, Q::State, usize)> = Vec::new();

        for (id, archetype) in &mut self.world.archetypes {
            if archetype.matches_mask(&query_mask) {
                let state = Q::init_state(archetype);
                let len = archetype.len();
                if len > 0 {
                    archetype_ranges.push((*id, state, len));
                }
            }
        }

        ParQueryIterMut {
            archetype_ranges,
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

/// Parallel iterator for mutable queries using Rayon
///
/// This iterator distributes work across multiple threads, processing
/// entities in parallel. Each archetype's entities are iterated in parallel.
pub struct ParQueryIterMut<'w, Q: QueryTarget> {
    /// (archetype_id, cached_state, entity_count)
    archetype_ranges: Vec<(ArchetypeId, Q::State, usize)>,
    _phantom: std::marker::PhantomData<&'w mut Q>,
}

// SAFETY: ParQueryIterMut can be sent between threads because:
// - archetype_ranges contains owned data (Vec) and raw pointers in Q::State
// - The raw pointers in Q::State point to component storage that remains valid
//   for the lifetime of the query (exclusive World access)
// - Each thread accesses different entity indices, so no data races occur
unsafe impl<'w, Q: QueryTarget> Send for ParQueryIterMut<'w, Q> where Q::State: Send {}
unsafe impl<'w, Q: QueryTarget> Sync for ParQueryIterMut<'w, Q> where Q::State: Sync {}

impl<'w, Q: QueryTarget> ParQueryIterMut<'w, Q>
where
    Q::State: Send + Sync,
    for<'a> Q::Item<'a>: Send,
{
    /// Get the number of threads available in Rayon's thread pool
    pub fn num_threads() -> usize {
        rayon::current_num_threads()
    }

    /// Get the total number of entities that will be processed
    pub fn entity_count(&self) -> usize {
        self.archetype_ranges.iter().map(|(_, _, len)| *len).sum()
    }

    /// Execute a closure on each entity in parallel
    ///
    /// This is the primary way to use parallel iteration. The closure
    /// receives the query result for each entity and can mutate components.
    ///
    /// # Example
    /// ```ignore
    /// query.par_iter_mut().for_each(|(transform, velocity)| {
    ///     transform.x += velocity.x * delta_time;
    /// });
    /// ```
    pub fn for_each<F>(self, f: F)
    where
        F: Fn(Q::Item<'_>) + Send + Sync,
    {
        // Process all archetypes in parallel
        self.archetype_ranges
            .into_par_iter()
            .for_each(|(_, state, len)| {
                // Process entities within each archetype in parallel
                (0..len).into_par_iter().for_each(|index| {
                    let item = Q::fetch_mut_with_state(&state, index);
                    f(item);
                });
            });
    }

    /// Execute a closure on each entity in parallel with custom batch size
    ///
    /// The `min_batch_size` parameter controls the minimum number of entities
    /// that will be processed by a single thread. Larger batches reduce overhead
    /// but may cause load imbalance. Smaller batches improve load balancing but
    /// increase overhead.
    ///
    /// # Guidelines
    /// - **Small work per entity** (simple math): use larger batches (1000-10000)
    /// - **Heavy work per entity** (complex calculations): use smaller batches (10-100)
    /// - **Default Rayon behavior**: ~1000 items per batch
    ///
    /// # Example
    /// ```ignore
    /// // Process in batches of at least 500 entities per thread
    /// query.par_iter_mut().for_each_batched(500, |(transform, velocity)| {
    ///     transform.x += velocity.x * delta_time;
    /// });
    /// ```
    pub fn for_each_batched<F>(self, min_batch_size: usize, f: F)
    where
        F: Fn(Q::Item<'_>) + Send + Sync,
    {
        self.archetype_ranges
            .into_par_iter()
            .for_each(|(_, state, len)| {
                // Use with_min_len to control batch size
                (0..len)
                    .into_par_iter()
                    .with_min_len(min_batch_size)
                    .for_each(|index| {
                        let item = Q::fetch_mut_with_state(&state, index);
                        f(item);
                    });
            });
    }

    /// Execute a closure on each entity in parallel with batch tracking
    ///
    /// Returns `BatchStats` containing information about how the work was
    /// distributed across batches. This is useful for performance tuning.
    ///
    /// # Example
    /// ```ignore
    /// let stats = query.par_iter_mut().for_each_tracked(|(transform, velocity)| {
    ///     transform.x += velocity.x * delta_time;
    /// });
    /// println!("Batches: {}, avg size: {}", stats.batch_count, stats.avg_batch_size);
    /// ```
    pub fn for_each_tracked<F>(self, f: F) -> BatchStats
    where
        F: Fn(Q::Item<'_>) + Send + Sync,
    {
        let num_threads = rayon::current_num_threads();
        let batch_count = Arc::new(AtomicUsize::new(0));
        let min_batch = Arc::new(AtomicUsize::new(usize::MAX));
        let max_batch = Arc::new(AtomicUsize::new(0));
        let total_entities = self.entity_count();

        self.archetype_ranges
            .into_par_iter()
            .for_each(|(_, state, len)| {
                // Each call to fold_with creates a batch
                let batch_count = Arc::clone(&batch_count);
                let min_batch = Arc::clone(&min_batch);
                let max_batch = Arc::clone(&max_batch);

                (0..len)
                    .into_par_iter()
                    .fold_with(0usize, |count, index| {
                        let item = Q::fetch_mut_with_state(&state, index);
                        f(item);
                        count + 1
                    })
                    .for_each(|batch_size| {
                        // This runs once per completed batch
                        batch_count.fetch_add(1, Ordering::Relaxed);
                        min_batch.fetch_min(batch_size, Ordering::Relaxed);
                        max_batch.fetch_max(batch_size, Ordering::Relaxed);
                    });
            });

        let batch_count = batch_count.load(Ordering::Relaxed);
        let min_batch_size = min_batch.load(Ordering::Relaxed);
        let max_batch_size = max_batch.load(Ordering::Relaxed);

        BatchStats {
            num_threads,
            batch_count,
            total_entities,
            min_batch_size: if batch_count > 0 { min_batch_size } else { 0 },
            max_batch_size,
            avg_batch_size: if batch_count > 0 {
                total_entities as f64 / batch_count as f64
            } else {
                0.0
            },
        }
    }

    /// Execute a closure on each entity in parallel with batch tracking and custom batch size
    ///
    /// Combines batch size control with batch tracking. Returns `BatchStats`
    /// containing information about how the work was distributed.
    ///
    /// # Example
    /// ```ignore
    /// let stats = query.par_iter_mut().for_each_batched_tracked(500, |(transform, velocity)| {
    ///     transform.x += velocity.x * delta_time;
    /// });
    /// println!("Batches: {}, min: {}, max: {}", stats.batch_count, stats.min_batch_size, stats.max_batch_size);
    /// ```
    pub fn for_each_batched_tracked<F>(self, min_batch_size: usize, f: F) -> BatchStats
    where
        F: Fn(Q::Item<'_>) + Send + Sync,
    {
        let num_threads = rayon::current_num_threads();
        let batch_count = Arc::new(AtomicUsize::new(0));
        let min_batch = Arc::new(AtomicUsize::new(usize::MAX));
        let max_batch = Arc::new(AtomicUsize::new(0));
        let total_entities = self.entity_count();

        self.archetype_ranges
            .into_par_iter()
            .for_each(|(_, state, len)| {
                let batch_count = Arc::clone(&batch_count);
                let min_batch = Arc::clone(&min_batch);
                let max_batch = Arc::clone(&max_batch);

                (0..len)
                    .into_par_iter()
                    .with_min_len(min_batch_size)
                    .fold_with(0usize, |count, index| {
                        let item = Q::fetch_mut_with_state(&state, index);
                        f(item);
                        count + 1
                    })
                    .for_each(|batch_size| {
                        batch_count.fetch_add(1, Ordering::Relaxed);
                        min_batch.fetch_min(batch_size, Ordering::Relaxed);
                        max_batch.fetch_max(batch_size, Ordering::Relaxed);
                    });
            });

        let batch_count = batch_count.load(Ordering::Relaxed);
        let min_batch_size_result = min_batch.load(Ordering::Relaxed);
        let max_batch_size = max_batch.load(Ordering::Relaxed);

        BatchStats {
            num_threads,
            batch_count,
            total_entities,
            min_batch_size: if batch_count > 0 {
                min_batch_size_result
            } else {
                0
            },
            max_batch_size,
            avg_batch_size: if batch_count > 0 {
                total_entities as f64 / batch_count as f64
            } else {
                0.0
            },
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
