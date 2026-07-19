//! [`QueryTarget`] trait and built-in implementations.
//!
//! A `QueryTarget` is the *data* shape a query yields per row. It can be
//! a single component reference (`&T` / `&mut T`), the [`Entity`] handle,
//! or a tuple combining several of these.

use trait_type_map::VecStorage;

use crate::archetype::Archetype;
use crate::component::{Component, ComponentId, ComponentTicks, Tick};
use crate::entity::Entity;

use super::change_detection::Mut;
use super::ptr::{SendPtr, SendPtrMut};

// ---------------------------------------------------------------------------
// Duplicate-write detection (guards against `Query<(&mut T, &mut T)>` UB)
// ---------------------------------------------------------------------------

/// Returns `true` when `writes` contains any duplicate [`ComponentId`],
/// which would mean a query tuple has two `&mut T` elements for the same
/// `T`. That pattern creates aliasing `&mut` references - UB.
#[inline]
pub(crate) fn has_duplicate_writes(writes: &[ComponentId]) -> bool {
    // Small-N linear scan: tuples are at most arity 4 (or 8 in the
    // future), so O(n²) with n ≤ 8 is cheaper than allocating a HashSet.
    for (i, a) in writes.iter().enumerate() {
        for b in &writes[i + 1..] {
            if a == b {
                return true;
            }
        }
    }
    false
}

/// Trait for fetching components from archetypes.
///
/// Implemented for:
/// - [`Entity`]: Access to entity IDs
/// - `&T`: Immutable component reference
/// - `&mut T`: Mutable component reference (yielded as [`Mut<T>`])
/// - Tuples up to arity 4
///
/// State is used to cache archetype-specific data (e.g. storage pointers)
/// for efficient access during parallel iteration. Each archetype that
/// matches a query has its state initialized once via [`init_state`].
///
/// [`init_state`]: QueryTarget::init_state
pub trait QueryTarget {
    type Item<'a>;
    type State;

    /// Get the component IDs required by this query.
    fn component_ids() -> Vec<ComponentId>;

    /// Report component access for system dependency analysis.
    /// Returns `(reads, writes)` as vectors of `ComponentId`.
    fn report_component_access() -> (Vec<ComponentId>, Vec<ComponentId>);

    /// Initialize state for fetching from an archetype (caches storage pointers).
    ///
    /// `this_run` is the current world tick used by mutable fetches to populate
    /// `Mut<T>::this_run`. Read-only targets ignore it.
    fn init_state(archetype: &mut Archetype, this_run: Tick) -> Self::State;

    /// Fetch components using cached state (used by both sequential and
    /// parallel iteration paths).
    fn fetch_with_state<'a>(state: &Self::State, index: usize) -> Self::Item<'a>;
}

// ----------------------------------------------------------------------------
// Entity
// ----------------------------------------------------------------------------

/// Allows queries to include `Entity` in the data tuple, e.g.
/// `Query<(Entity, &Transform)>` to get the entity along with its components.
impl QueryTarget for Entity {
    type Item<'a> = Entity;
    type State = SendPtr<Vec<Entity>>;

    fn component_ids() -> Vec<ComponentId> {
        Vec::new()
    }

    fn report_component_access() -> (Vec<ComponentId>, Vec<ComponentId>) {
        (Vec::new(), Vec::new())
    }

    fn init_state(archetype: &mut Archetype, _this_run: Tick) -> Self::State {
        let _zone = crate::profile_scope!(
            "create state for entity query target",
            [("Entities in this archetype: {}", archetype.entity_count())]
        );
        SendPtr::new(&archetype.entities as *const Vec<Entity>)
    }

    fn fetch_with_state<'a>(state: &Self::State, index: usize) -> Self::Item<'a> {
        unsafe { *(&*state.as_ptr()).get_unchecked(index) }
    }
}

// ----------------------------------------------------------------------------
// &T (immutable component reference)
// ----------------------------------------------------------------------------

impl<T: Component> QueryTarget for &T {
    type Item<'a> = &'a T;
    type State = SendPtr<VecStorage<T, dyn Component>>;

    fn component_ids() -> Vec<ComponentId> {
        vec![ComponentId::of::<T>()]
    }

    fn report_component_access() -> (Vec<ComponentId>, Vec<ComponentId>) {
        (vec![ComponentId::of::<T>()], Vec::new())
    }

    fn init_state(archetype: &mut Archetype, _this_run: Tick) -> Self::State {
        let _zone = crate::profile_scope!(
            "create state for immutable component query target",
            [("Entities in archetype: {}", archetype.entity_count()), ("Component type: {}", std::any::type_name::<T>())]
        );
        SendPtr::new(
            archetype.component_storages.get_storage::<T>() as *const VecStorage<T, dyn Component>
        )
    }

    fn fetch_with_state<'a>(state: &Self::State, index: usize) -> Self::Item<'a> {
        // SAFETY: The query loop invariant guarantees `index < archetype.len()`,
        // and the ECS invariant guarantees `archetype.len() == storage.len()`
        // for every component type in the archetype. Therefore `index` is
        // always in bounds for this storage.
        unsafe { (*state.as_ptr()).get_unchecked(index) }
    }
}

// ----------------------------------------------------------------------------
// &mut T (mutable component reference yielded as Mut<T>)
// ----------------------------------------------------------------------------

/// Cached pointers used by mutable component queries to construct `Mut<T>`
/// without re-locating the underlying storage on every access.
pub struct MutFetchState<T: Component> {
    values: SendPtrMut<VecStorage<T, dyn Component>>,
    ticks: SendPtrMut<Vec<ComponentTicks>>,
    this_run: Tick,
}

// SAFETY: Both inner pointers wrap raw addresses backed by storage that
// outlives the query. Disjoint per-row access is guaranteed by the scheduler.
unsafe impl<T: Component> Send for MutFetchState<T> {}
unsafe impl<T: Component> Sync for MutFetchState<T> {}

impl<T: Component> QueryTarget for &mut T {
    type Item<'a> = Mut<'a, T>;
    type State = MutFetchState<T>;

    fn component_ids() -> Vec<ComponentId> {
        vec![ComponentId::of::<T>()]
    }

    fn report_component_access() -> (Vec<ComponentId>, Vec<ComponentId>) {
        (Vec::new(), vec![ComponentId::of::<T>()])
    }

    fn init_state(archetype: &mut Archetype, this_run: Tick) -> Self::State {
        let _zone = crate::profile_scope!(
            "create state for mutable component query target",
            [("Entities in archetype: {}", archetype.entity_count()), ("Component type (mutable): {}", std::any::type_name::<T>())]
        );
        let values = SendPtrMut::new(archetype.component_storages.get_storage_mut::<T>()
            as *mut VecStorage<T, dyn Component>);
        let ticks_vec = archetype
            .component_ticks
            .get_mut(&ComponentId::of::<T>())
            .expect("component_ticks vec missing for type - archetype not properly initialized")
            as *mut Vec<ComponentTicks>;
        let ticks = SendPtrMut::new(ticks_vec);
        MutFetchState {
            values,
            ticks,
            this_run,
        }
    }

    fn fetch_with_state<'a>(state: &Self::State, index: usize) -> Self::Item<'a> {
        // SAFETY: Disjoint per-row access guaranteed by the scheduler. Both
        // pointers are valid for the lifetime of the iteration. The query loop
        // invariant guarantees `index < archetype.len() == storage.len()`,
        // so unchecked access is sound. Mutating through Mut::deref_mut
        // updates ticks[index].changed without requiring atomics because
        // no other thread observes this row.
        unsafe {
            let value: &'a mut T = (*state.values.as_ptr()).get_mut_unchecked(index);
            let ticks: &'a mut ComponentTicks =
                &mut *(*state.ticks.as_ptr()).as_mut_ptr().add(index);
            Mut::new(value, ticks, state.this_run)
        }
    }
}

// ----------------------------------------------------------------------------
// Tuple Implementations (via macro)
// ----------------------------------------------------------------------------

/// Implements `QueryTarget` for tuples up to arity 4, e.g.
/// `(Entity, &Transform, &mut Velocity)`.
macro_rules! impl_query_target_tuple {
    ($($T:ident),*) => {
        impl<$($T: QueryTarget),*> QueryTarget for ($($T,)*) {
            type Item<'a> = ($($T::Item<'a>,)*);
            type State = ($($T::State,)*);

            fn component_ids() -> Vec<ComponentId> {
                let mut ids = Vec::with_capacity(crate::config::QueryConfig::DEFAULT_TUPLE_COMPONENT_IDS_CAPACITY);
                $(ids.extend($T::component_ids());)*
                ids
            }

            fn report_component_access() -> (Vec<ComponentId>, Vec<ComponentId>) {
                let mut reads = Vec::with_capacity(crate::config::QueryConfig::DEFAULT_TUPLE_COMPONENT_IDS_CAPACITY);
                let mut writes = Vec::with_capacity(crate::config::QueryConfig::DEFAULT_TUPLE_COMPONENT_IDS_CAPACITY);
                $(
                    let (r, w) = $T::report_component_access();
                    reads.extend(r);
                    writes.extend(w);
                )*

                // Check for duplicate mutable component types in the tuple, which would create aliasing `&mut` references - UB.
                debug_assert!(
                    !$crate::query::target::has_duplicate_writes(&writes),
                    "Query tuple contains duplicate mutable component types \
                     (e.g. Query<(&mut T, &mut T)>). This is not allowed"
                );
                (reads, writes)
            }

            #[allow(non_snake_case)]
            fn init_state(archetype: &mut Archetype, this_run: Tick) -> Self::State {
                let _zone = crate::profile_scope!(
                    "create state for tuple query target",
                    [("Entities in archetype: {}", archetype.entity_count())]
                );
                let archetype_ptr = archetype as *mut Archetype;
                unsafe { ($($T::init_state(&mut *archetype_ptr, this_run),)*) }
            }

            #[allow(non_snake_case)]
            fn fetch_with_state<'a>(state: &Self::State, index: usize) -> Self::Item<'a> {
                let ($($T,)*) = state;
                ($($T::fetch_with_state($T, index),)*)
            }
        }
    };
}

impl_query_target_tuple!(A);
impl_query_target_tuple!(A, B);
impl_query_target_tuple!(A, B, C);
impl_query_target_tuple!(A, B, C, D);
impl_query_target_tuple!(A, B, C, D, E);
