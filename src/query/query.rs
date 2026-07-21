//! The [`Query`] entry point — holds an exclusive borrow of the [`World`]
//! and produces sequential or parallel iterators.
//!
//! # Responsibilities
//!
//! - Constructs queries from a `&mut World` and cached archetype lists.
//! - Builds [`ComponentMask`] for target and filter types to skip non-matching archetypes.
//! - Delegates iteration to [`QueryIterMut`] (sequential) or [`ParQueryIter`] (parallel).
//!
//! # Design
//!
//! The query caches matching archetype IDs with a generation counter so
//! subsequent calls to [`iter_mut`](Query::iter_mut) avoid rescanning when
//! no archetypes were added or removed since the last call.

// Standard library

// Current crate
use crate::archetype::{Archetype, ArchetypeId};
use crate::component::ComponentMask;
use crate::world::World;

use super::filter::QueryFilter;
use super::iter::{ParQueryIter, QueryIterMut};
use super::target::QueryTarget;
use super::FilteredArchetypeRange;

// =============================================================================
// Query
// =============================================================================

/// Query for iterating over entities matching a component pattern.
///
/// The query is parameterized by two type arguments:
///
/// - `Q: QueryTarget` - the data fetched per row (e.g.
///   `(Entity, &Transform, &mut Velocity)`).
/// - `F: QueryFilter` - an optional row predicate (default `()`, accepts
///   everything).
///
/// # Examples
/// ```no_run
/// # use ecs_hybrid::*;
/// # #[derive(Debug, Clone)] struct Transform { x: f32, y: f32 }
/// # impl Component for Transform {}
/// # #[derive(Debug, Clone)] struct Velocity { vx: f32, vy: f32 }
/// # impl Component for Velocity {}
/// # #[derive(Debug, Clone)] struct Health(f32);
/// # impl Component for Health {}
/// # #[derive(Debug, Clone)] struct Enemy;
/// # impl Component for Enemy {}
/// # #[derive(Debug, Clone)] struct Frozen;
/// # impl Component for Frozen {}
/// // Plain query - data only
/// fn movement(mut q: Query<(&mut Transform, &Velocity)>) { /* ... */ }
///
/// // Filter on change detection
/// fn react_to_moves(mut q: Query<(Entity, &Transform), Changed<Transform>>) {
///     for (e, t) in q.iter_mut() {
///         println!("entity {:?} moved to {:?}", e.id(), (t.x, t.y));
///     }
/// }
///
/// // With/Without to scope the iteration
/// fn enemies_only(mut q: Query<&Health, (With<Enemy>, Without<Frozen>)>) {
///     /* ... */
/// }
/// ```
pub struct Query<'w, Q: QueryTarget, F: QueryFilter = ()> {
    world: &'w mut World,
    /// Cached component mask for the query target (data being fetched).
    /// Computed once at construction - static for a given `Q`.
    target_mask: ComponentMask,
    /// Cached filter mask pairs. Computed once at construction - static
    /// for a given `F`.  For simple filters this is a single pair; only
    /// [`Or`] filters produce multiple pairs.
    filter_pairs: Vec<(ComponentMask, ComponentMask)>,
    /// Sum of all queried component sizes in bytes — cached for slice clamping.
    total_components_size: usize,
    /// Cached list of matching archetype IDs. Valid when
    /// `cached_generation == world.archetype_generation`.
    cached_matches: Vec<ArchetypeId>,
    cached_generation: u64,
    _phantom: std::marker::PhantomData<(Q, F)>,
}

impl<'w, Q: QueryTarget, F: QueryFilter> Query<'w, Q, F> {
    pub fn new(world: &'w mut World) -> Self {
        let _zone = crate::profile_scope!(
            "create query",
            [
                ("Query target component types: {}", Q::component_ids().len()),
                (
                    "Filter archetype mask pairs: {}",
                    F::archetype_filter_pairs().len()
                )
            ]
        );

        let target_mask = Self::build_target_mask(world);
        let filter_pairs = Self::build_filter_mask_pairs(world);
        let total_components_size = Q::component_ids()
            .iter()
            .filter_map(|id| world.component_registry.get_size(id))
            .sum::<usize>()
            .max(8);
        Self {
            world,
            target_mask,
            filter_pairs,
            total_components_size,
            cached_matches: Vec::new(),
            cached_generation: 0,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Build the component mask for the query target from the world's
    /// component registry.  Called once during [`new`](Self::new).
    fn build_target_mask(world: &World) -> ComponentMask {
        let mut mask = ComponentMask::empty();
        for component_id in &Q::component_ids() {
            if let Some(bit) = world.component_registry.get_bit(component_id) {
                mask.set(bit);
            }
        }
        mask
    }

    /// Build filter mask pairs from the world's component registry.
    /// Called once during [`new`](Self::new).
    fn build_filter_mask_pairs(world: &World) -> Vec<(ComponentMask, ComponentMask)> {
        let _zone = crate::profile_scope!(
            "build component query filter mask",
            [(
                "Filter archetype mask pairs to evaluate: {}",
                F::archetype_filter_pairs().len()
            )]
        );

        let registry = &world.component_registry;
        F::archetype_filter_pairs()
            .into_iter()
            .map(|(included_ids, excluded_ids)| {
                let mut included_mask = ComponentMask::empty();
                let mut excluded_mask = ComponentMask::empty();
                for component_id in &included_ids {
                    if let Some(bit) = registry.get_bit(component_id) {
                        included_mask.set(bit);
                    }
                }
                for component_id in &excluded_ids {
                    if let Some(bit) = registry.get_bit(component_id) {
                        excluded_mask.set(bit);
                    }
                }
                (included_mask, excluded_mask)
            })
            .collect()
    }

    /// True if `archetype` matches this query's requirements.
    ///
    /// The check has two layers:
    ///
    /// 1. **Target mask** (hard AND): the archetype must contain every
    ///    component the query fetches (e.g. `&Position` + `&mut Velocity`).
    ///
    /// 2. **Filter pairs** (OR across pairs, AND within each pair):
    ///    - **0 pairs** (e.g. no filter, or `()`): any archetype with the
    ///      target components matches.
    ///    - **1 pair** (e.g. `With<A>`, `Without<B>`, `Changed<A>`):
    ///      the archetype must have all `include` components AND none of
    ///      the `exclude` components. This is the simple case - just like
    ///      the old `(include_mask, exclude_mask)` model.
    ///    - **2+ pairs** (only [`Or`] filters): the archetype matches if
    ///      **any** pair matches. For `Or<(With<A>, With<B>)>` the pairs
    ///      are `({A},{})` and `({B},{})`, so archetypes with A, B, or
    ///      both are included.
    fn archetype_matches(
        archetype: &Archetype,
        target_mask: &ComponentMask,
        filter_pairs: &[(ComponentMask, ComponentMask)],
    ) -> bool {
        // Every matching archetype must contain the data we're fetching.
        if !archetype.matches_mask(target_mask) {
            return false;
        }

        match filter_pairs.len() {
            // No filter restrictions - any archetype with the target components passes.
            0 => true,

            // Fast path: the common case. One include+exclude pair.
            // Conceptually identical to the old `(include, exclude)` model.
            1 => {
                let (included_mask, excluded_mask) = &filter_pairs[0];
                archetype.matches_mask(included_mask)
                    && (excluded_mask.is_empty()
                        || ComponentMask::intersection(&archetype.component_mask, excluded_mask)
                            .is_empty())
            }

            // Only reached for `Or<...>` filters. Match if ANY pair matches.
            _ => filter_pairs.iter().any(|(included_mask, excluded_mask)| {
                archetype.matches_mask(included_mask)
                    && (excluded_mask.is_empty()
                        || ComponentMask::intersection(&archetype.component_mask, excluded_mask)
                            .is_empty())
            }),
        }
    }

    /// Get the list of archetype IDs matching this query, using a
    /// generation-based cache to avoid rescanning when archetypes
    /// haven't changed since the last call.
    #[inline]
    fn matching_archetype_ids(&mut self) -> &[ArchetypeId] {
        if self.cached_generation != self.world.archetype_generation {
            let _zone = crate::profile_scope!(
                "find matching archetypes",
                [
                    ("All archetypes in world: {}", self.world.archetypes.len()),
                    ("Cached archetype generation: {}", self.cached_generation),
                    (
                        "Current archetype generation: {}",
                        self.world.archetype_generation
                    )
                ]
            );
            let mut matching: Vec<ArchetypeId> = self
                .world
                .archetypes
                .iter()
                .filter(|(_, arch)| {
                    Self::archetype_matches(arch, &self.target_mask, &self.filter_pairs)
                })
                .map(|(id, _)| *id)
                .collect();
            matching.sort();
            crate::profile_message!(
                "query archetype cache miss: generation {} -> {}, rescanned {} archetypes -> {} matched for this query",
                self.cached_generation,
                self.world.archetype_generation,
                self.world.archetypes.len(),
                matching.len(),
            );
            self.cached_matches = matching;
            self.cached_generation = self.world.archetype_generation;
        }
        &self.cached_matches
    }

    /// Create a sequential iterator over all matching entities.
    #[inline]
    pub fn iter_mut(&mut self) -> QueryIterMut<'_, Q, F> {
        let _zone = crate::profile_scope!(
            "create sequential query iterator",
            [(
                "Archetypes matching this query: {}",
                self.matching_archetype_ids().len()
            )]
        );

        let this_run = self.world.increment_change_tick();
        let last_run = self.world.system_last_run();
        let matching = self.matching_archetype_ids().to_vec();

        QueryIterMut::new(self.world as *mut World, matching, this_run, last_run)
    }

    /// Get the first matching entity's components.
    ///
    /// For unfiltered queries, finds the first non-empty matching archetype
    /// directly. For filtered queries, falls back to the iterator.
    #[inline]
    pub fn first(&mut self) -> Option<Q::Item<'_>> {
        // Fast path: unfiltered query - grab the first entity from the first non-empty matching archetype.
        if self.filter_pairs.is_empty() {
            let this_run = self.world.increment_change_tick();
            for archetype in self.world.archetypes.values_mut() {
                if archetype.matches_mask(&self.target_mask) && !archetype.is_empty() {
                    let state = Q::init_state(archetype, this_run);
                    return Some(Q::fetch_with_state(&state, 0));
                }
            }
            return None;
        }
        self.iter_mut().next()
    }

    /// Check if any entity matches this query.
    ///
    /// For unfiltered queries, this is O(archetypes).  
    /// For filtered queries, falls back to checking the first matching row.
    #[inline]
    pub fn is_empty(&mut self) -> bool {
        // Fast path: unfiltered query - just check if any matching archetype has entities.
        if self.filter_pairs.is_empty() {
            return !self
                .world
                .archetypes
                .values()
                .any(|arch| arch.matches_mask(&self.target_mask) && !arch.is_empty());
        }
        // Slow path: need per-row filter evaluation.
        self.iter_mut().next().is_none()
    }

    /// Count the number of entities matching this query.
    ///
    /// For unfiltered queries (`F = ()`), this is O(archetypes) - sums
    /// archetype entity counts directly without iterating rows.  For
    /// filtered queries (e.g. `Changed<T>`), falls back to O(n) row-by-row
    /// counting since each row must be tested individually.
    #[inline]
    pub fn entity_count(&mut self) -> usize {
        // Fast path: unfiltered query - just sum archetype lengths.
        if self.filter_pairs.is_empty() {
            return self
                .world
                .archetypes
                .values()
                .filter(|arch| arch.matches_mask(&self.target_mask))
                .map(|arch| arch.len())
                .sum();
        }
        // Slow path: need per-row filter evaluation.
        self.iter_mut().count()
    }

    /// Create a parallel iterator over all matching entities.
    #[inline]
    pub fn par_iter_mut(&mut self) -> ParQueryIter<'_, Q, F>
    where
        Q::State: Send + Sync,
        F::State: Send + Sync,
        for<'a> Q::Item<'a>: Send,
    {
        let this_run = self.world.increment_change_tick();
        let last_run = self.world.system_last_run();

        // Use cached archetype IDs to avoid rescanning, but initialize
        // states fresh (they capture the current tick).
        let matching_ids = self.matching_archetype_ids().to_vec();
        let _zone = crate::profile_scope!(
            "create parallel query iterator",
            [("Archetypes matching this query: {}", matching_ids.len())]
        );
        let archetype_ranges: Vec<FilteredArchetypeRange<Q::State, F::State>> = matching_ids
            .iter()
            .filter_map(|id| {
                self.world.archetypes.get_mut(id).map(|arch| {
                    let archetype_ptr = arch as *mut Archetype;
                    let q_state = unsafe { Q::init_state(&mut *archetype_ptr, this_run) };
                    let f_state = unsafe { F::init_state(&mut *archetype_ptr, last_run, this_run) };
                    let len = arch.len();
                    (*id, q_state, f_state, len)
                })
            })
            .filter(|(_, _, _, len)| *len > 0)
            .collect();
        // Matching IDs are already sorted by matching_archetype_ids(), and
        // filter_map preserves iteration order, so archetype_ranges is
        // already sorted — no need for an extra sort_by_key.

        ParQueryIter::new(
            archetype_ranges,
            self.world.iterator_timings.clone(),
            self.total_components_size,
        )
    }
}
