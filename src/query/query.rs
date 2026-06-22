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

    /// Build the component mask for the query target (data being fetched).
    /// This is always mandatory — every matching archetype must contain
    /// all components in this mask.
    ///
    /// This is separate from filter requirements so that [`Or`] filters can
    /// add their own include/exclude pairs with OR semantics, while the
    /// data we fetch remains a hard AND requirement.
    fn build_target_mask(&self) -> ComponentMask {
        let mut mask = ComponentMask::empty();
        for component_id in &Q::component_ids() {
            if let Some(bit) = self.world.component_registry.get_bit(component_id) {
                mask.set(bit);
            }
        }
        mask
    }

    /// Convert the filter's archetype-level requirements into
    /// `(include_mask, exclude_mask)` pairs.
    ///
    /// For simple filters like [`With<A>`] or [`Without<B>`] this returns
    /// exactly **one** pair — the same include/exclude logic as before.
    /// Only [`Or`] filters return multiple pairs (one per inner filter),
    /// enabling correct logical-OR at the archetype level.
    fn build_filter_mask_pairs(&self) -> Vec<(ComponentMask, ComponentMask)> {
        let registry = &self.world.component_registry;
        F::archetype_filter_pairs()
            .into_iter()
            .map(|(inc_ids, exc_ids)| {
                let mut inc = ComponentMask::empty();
                let mut exc = ComponentMask::empty();
                for id in &inc_ids {
                    if let Some(bit) = registry.get_bit(id) {
                        inc.set(bit);
                    }
                }
                for id in &exc_ids {
                    if let Some(bit) = registry.get_bit(id) {
                        exc.set(bit);
                    }
                }
                (inc, exc)
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
    ///      the `exclude` components. This is the simple case — just like
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
            // No filter restrictions — any archetype with the target components passes.
            0 => true,

            // Fast path: the common case. One include+exclude pair.
            // Conceptually identical to the old `(include, exclude)` model.
            1 => {
                let (inc, exc) = &filter_pairs[0];
                archetype.matches_mask(inc)
                    && (exc.is_empty()
                        || ComponentMask::intersection(&archetype.component_mask, exc).is_empty())
            }

            // Only reached for `Or<...>` filters. Match if ANY pair matches.
            _ => filter_pairs.iter().any(|(inc, exc)| {
                archetype.matches_mask(inc)
                    && (exc.is_empty()
                        || ComponentMask::intersection(&archetype.component_mask, exc).is_empty())
            }),
        }
    }

    /// Create a sequential iterator over all matching entities.
    #[inline]
    pub fn iter_mut(&mut self) -> QueryIterMut<'_, Q, F> {
        let target_mask = self.build_target_mask();
        let filter_pairs = self.build_filter_mask_pairs();
        let this_run = self.world.increment_change_tick();
        let last_run = self.world.system_last_run();

        let mut matching_archetypes: Vec<ArchetypeId> = self
            .world
            .archetypes
            .iter()
            .filter(|(_, arch)| Self::archetype_matches(arch, &target_mask, &filter_pairs))
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
        let target_mask = self.build_target_mask();
        let filter_pairs = self.build_filter_mask_pairs();
        let this_run = self.world.increment_change_tick();
        let last_run = self.world.system_last_run();

        let mut archetype_ranges: Vec<FilteredArchetypeRange<Q::State, F::State>> = self
            .world
            .archetypes
            .iter_mut()
            .filter(|(_, arch)| Self::archetype_matches(arch, &target_mask, &filter_pairs))
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
