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
    _phantom: std::marker::PhantomData<(Q, F)>,
}

impl<'w, Q: QueryTarget, F: QueryFilter> Query<'w, Q, F> {
    pub fn new(world: &'w mut World) -> Self {
        let target_mask = Self::build_target_mask(world);
        let filter_pairs = Self::build_filter_mask_pairs(world);
        Self {
            world,
            target_mask,
            filter_pairs,
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

    /// Create a sequential iterator over all matching entities.
    #[inline]
    pub fn iter_mut(&mut self) -> QueryIterMut<'_, Q, F> {
        let this_run = self.world.increment_change_tick();
        let last_run = self.world.system_last_run();

        let mut matching_archetypes: Vec<ArchetypeId> = self
            .world
            .archetypes
            .iter()
            .filter(|(_, arch)| {
                Self::archetype_matches(arch, &self.target_mask, &self.filter_pairs)
            })
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
        let this_run = self.world.increment_change_tick();
        let last_run = self.world.system_last_run();

        let mut archetype_ranges: Vec<FilteredArchetypeRange<Q::State, F::State>> = self
            .world
            .archetypes
            .iter_mut()
            .filter(|(_, arch)| {
                Self::archetype_matches(arch, &self.target_mask, &self.filter_pairs)
            })
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
