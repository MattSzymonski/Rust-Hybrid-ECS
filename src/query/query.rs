//! The [`Query`] entry point - holds an exclusive borrow of the [`World`]
//! and produces sequential or parallel iterators.

use crate::archetype::{Archetype, ArchetypeId};
use crate::component::ComponentMask;
use crate::world::World;

use super::filter::QueryFilter;
use super::iter::{ParQueryIter, QueryIterMut};
use super::target::QueryTarget;
use super::FilteredArchetypeRange;

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
/// ```ignore
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
    _phantom: std::marker::PhantomData<(Q, F)>,
}

impl<'w, Q: QueryTarget, F: QueryFilter> Query<'w, Q, F> {
    pub fn new(world: &'w mut World) -> Self {
        Self {
            world,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Build the union of components that the data target and the filter
    /// require to be present (i.e. the inclusion mask).
    fn build_query_mask(&self) -> ComponentMask {
        let mut mask = ComponentMask::empty();
        for component_id in Q::component_ids()
            .iter()
            .chain(F::included_component_ids().iter())
        {
            if let Some(bit) = self.world.component_registry.get_bit(component_id) {
                mask.set(bit);
            }
        }
        mask
    }

    /// Build the mask of components whose presence excludes an archetype.
    fn build_exclusion_mask(&self) -> ComponentMask {
        let mut mask = ComponentMask::empty();
        for component_id in &F::excluded_component_ids() {
            if let Some(bit) = self.world.component_registry.get_bit(component_id) {
                mask.set(bit);
            }
        }
        mask
    }

    /// True if `archetype` matches both the inclusion and exclusion masks.
    fn archetype_matches(
        archetype: &Archetype,
        include: &ComponentMask,
        exclude: &ComponentMask,
    ) -> bool {
        if !archetype.matches_mask(include) {
            return false;
        }
        // No bit in `exclude` may be set in the archetype's mask.
        let am = &archetype.component_mask;
        let combined = ComponentMask::intersection(am, exclude);
        combined.is_empty()
    }

    /// Create a sequential iterator over all matching entities.
    #[inline]
    pub fn iter_mut(&mut self) -> QueryIterMut<'_, Q, F> {
        let include = self.build_query_mask();
        let exclude = self.build_exclusion_mask();
        let this_run = self.world.increment_change_tick();
        let last_run = self.world.system_last_run();

        let mut matching_archetypes: Vec<ArchetypeId> = self
            .world
            .archetypes
            .iter()
            .filter(|(_, arch)| Self::archetype_matches(arch, &include, &exclude))
            .map(|(id, _)| *id)
            .collect();
        // Sort by ArchetypeId for deterministic iteration order across runs.
        matching_archetypes.sort();

        QueryIterMut::new(
            self.world as *mut World,
            matching_archetypes,
            this_run,
            last_run,
        )
    }

    /// Get the first matching entity's components.
    #[inline]
    pub fn first(&mut self) -> Option<Q::Item<'_>> {
        self.iter_mut().next()
    }

    /// Check if any entity matches this query.
    #[inline]
    pub fn is_empty(&mut self) -> bool {
        self.iter_mut().next().is_none()
    }

    /// Count the number of entities matching this query.
    ///
    /// When the filter is non-trivial (e.g. `Changed<T>`), this is O(n)
    /// over candidate rows because each row must be tested individually.
    #[inline]
    pub fn entity_count(&mut self) -> usize {
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
        let include = self.build_query_mask();
        let exclude = self.build_exclusion_mask();
        let this_run = self.world.increment_change_tick();
        let last_run = self.world.system_last_run();

        let mut archetype_ranges: Vec<FilteredArchetypeRange<Q::State, F::State>> = self
            .world
            .archetypes
            .iter_mut()
            .filter(|(_, arch)| Self::archetype_matches(arch, &include, &exclude))
            .filter(|(_, arch)| !arch.is_empty())
            .map(|(id, arch)| {
                let arch_ptr = arch as *mut Archetype;
                // SAFETY: We have exclusive access to `arch` and only call
                // each init_state once.
                let q_state = unsafe { Q::init_state(&mut *arch_ptr, this_run) };
                let f_state = unsafe { F::init_state(&mut *arch_ptr, last_run, this_run) };
                let len = arch.len();
                (*id, q_state, f_state, len)
            })
            .collect();
        // Sort by ArchetypeId for deterministic iteration order across runs.
        archetype_ranges.sort_by_key(|(id, _, _, _)| *id);

        ParQueryIter::new(archetype_ranges)
    }
}
