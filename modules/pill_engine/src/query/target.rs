//! [`QueryTarget`] trait and built-in implementations for query data shapes.
//!
//! # Responsibilities
//!
//! - Defines the [`QueryTarget`] trait that determines what data a query yields per row.
//! - Implements `QueryTarget` for `&T`, `&mut T`, [`Entity`], and tuples of these.
//! - Detects duplicate mutable borrows (`Query<(&mut T, &mut T)>`) at construction time.
//!
//! # Design
//!
//! Each `QueryTarget` implementation reports its component IDs (for mask building),
//! creates per-archetype state (raw pointers into component storage), and fetches
//! one row of data at a given index. For parallel iteration, the state must be
//! `Send + Sync` so it can be shared across Rayon threads.

// External crates
use trait_type_map::ErasedVecStorage;

// Current crate
use super::change_detection::Mut;
use super::ptr::{SendPtr, SendPtrMut};
use crate::archetype::Archetype;
use crate::component::{Component, ComponentId, ComponentTicks, Tick};
use crate::entity::Entity;

// =============================================================================
// QueryTarget
// =============================================================================

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
    /// The type yielded per row by this query target.
    type Item<'a>;
    /// Cached per-archetype state (e.g. storage pointers) for this target.
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

// =============================================================================
// Entity
// =============================================================================

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
        // SAFETY: The query loop invariant guarantees `index <
        // archetype.len()`, and the archetype's entity vector length always
        // equals its entity count, so the unchecked index is in bounds. The
        // pointer was captured from `&archetype.entities` in `init_state`
        // and the entity storage is not reallocated during iteration, so
        // the reference is valid for `'a`. `Entity` is `Copy`, so copying
        // the value out cannot create aliasing references.
        unsafe { *(&*state.as_ptr()).get_unchecked(index) }
    }
}

// =============================================================================
// &T (Immutable Reference)
// =============================================================================

/// Allows queries to include immutable component references, e.g.
/// `Query<(&Transform, &Velocity)>`. Read-only access yields a plain `&T`
/// and reports the component as a read for system dependency analysis.
impl<T: Component> QueryTarget for &T {
    type Item<'a> = &'a T;
    type State = SendPtr<ErasedVecStorage<dyn Component>>;

    fn component_ids() -> Vec<ComponentId> {
        vec![ComponentId::of::<T>()]
    }

    fn report_component_access() -> (Vec<ComponentId>, Vec<ComponentId>) {
        (vec![ComponentId::of::<T>()], Vec::new())
    }

    fn init_state(archetype: &mut Archetype, _this_run: Tick) -> Self::State {
        let _zone = crate::profile_scope!(
            "create state for immutable component query target",
            [
                ("Entities in archetype: {}", archetype.entity_count()),
                ("Component type: {}", std::any::type_name::<T>())
            ]
        );
        SendPtr::new(archetype.component_storages.get_storage::<T>()
            as *const ErasedVecStorage<dyn Component>)
    }

    fn fetch_with_state<'a>(state: &Self::State, index: usize) -> Self::Item<'a> {
        // SAFETY: The query loop invariant guarantees `index < archetype.len()`,
        // and the ECS invariant guarantees `archetype.len() == storage.len()`
        // for every component type in the archetype. Therefore `index` is
        // always in bounds for this storage.
        unsafe { (*state.as_ptr()).get_unchecked::<T>(index) }
    }
}

// =============================================================================
// MutFetchState
// =============================================================================

/// Cached pointers used by mutable component queries to construct `Mut<T>`
/// without re-locating the underlying storage on every access.
pub struct MutFetchState<T: Component> {
    /// Raw pointer to the component values storage, cached to avoid
    /// re-locating the storage on every row fetch.
    values: SendPtrMut<ErasedVecStorage<dyn Component>>,
    /// Raw pointer to the per-entity change-detection ticks storage.
    ticks: SendPtrMut<Vec<ComponentTicks>>,
    /// The world tick for this run, stored on `Mut<T>` at fetch time.
    this_run: Tick,
    /// Ties the state to the queried component type.
    _marker: std::marker::PhantomData<T>,
}

// SAFETY: Both inner pointers wrap raw addresses backed by storage that
// outlives the query.
//
// Disjoint access is established by `SystemAccess::conflicts_with`, which the
// scheduler consults before placing two systems in the same parallel batch; it
// only takes its bitmask fast path when both systems' masks are complete, and
// otherwise compares the full access sets. That completeness rule is what makes
// this impl sound - an earlier version inferred "no access" from an empty mask,
// so two systems writing the same component could be batched together and this
// state shared across threads for genuinely overlapping rows.
//
// Within one batch, `ParQueryIter` assigns each thread a disjoint entity range,
// so no two threads fetch the same row.
unsafe impl<T: Component> Send for MutFetchState<T> {}
unsafe impl<T: Component> Sync for MutFetchState<T> {}

/// Allows queries to include mutable component references, e.g.
/// `Query<&mut Velocity>`. Mutable access yields [`Mut<T>`] with change
/// detection and reports the component as a write for system dependency
/// analysis.
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
            [
                ("Entities in archetype: {}", archetype.entity_count()),
                ("Component type (mutable): {}", std::any::type_name::<T>())
            ]
        );
        let values = SendPtrMut::new(archetype.component_storages.get_storage_mut::<T>()
            as *mut ErasedVecStorage<dyn Component>);
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
            _marker: std::marker::PhantomData,
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
            let value: &'a mut T = (*state.values.as_ptr()).get_mut_unchecked::<T>(index);
            let ticks: &'a mut ComponentTicks =
                &mut *(*state.ticks.as_ptr()).as_mut_ptr().add(index);
            Mut::new(value, ticks, state.this_run)
        }
    }
}

// =============================================================================
// Tuple Implementations
// =============================================================================

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
                // SAFETY: Each tuple element re-derives its own `&mut
                // Archetype` from the same raw pointer and uses it only
                // within its own `init_state` call, so the mutable borrows
                // never overlap. The archetype is a `&mut` parameter that
                // outlives the whole call, so the pointer remains valid for
                // every element. `init_state` only reads out storage
                // pointers and never reallocates or otherwise invalidates
                // the archetype, so all derived references stay valid.
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

// =============================================================================
// Free Functions
// =============================================================================

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
