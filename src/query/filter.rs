//! [`QueryFilter`] trait and built-in filter types.
//!
//! Filters are composable predicates that decide which entities a query
//! yields beyond the basic component-set match implied by the
//! [`QueryTarget`].
//!
//! Built-in filters:
//!
//! - [`With<T>`] / [`Without<T>`] - archetype-level scoping
//! - [`Changed<T>`] / [`Added<T>`] - row-level change-detection
//! - Tuples - logical AND of inner filters
//! - [`Or`] - logical OR of inner filters

use crate::archetype::Archetype;
use crate::component::{Component, ComponentId, ComponentTicks, Tick};

use super::ptr::SendPtr;

// ============================================================================
// QueryFilter Trait
// ============================================================================

/// Trait for predicates that decide which entities a query yields beyond
/// the basic component-set match implied by the [`QueryTarget`].
///
/// Filters operate at two levels:
///
/// 1. **Archetype-level**: [`included_component_ids`] adds required
///    components to the query mask, and [`excluded_component_ids`] excludes
///    archetypes that contain any of the listed components. This is enough
///    to handle [`With`] and [`Without`].
///
/// 2. **Row-level**: [`init_state`] caches per-archetype data (e.g. a
///    pointer into `component_ticks`) and [`matches`] is invoked for each
///    candidate row. This drives [`Changed`] and [`Added`].
///
/// The trivial filter `()` matches every row.
///
/// [`QueryTarget`]: super::QueryTarget
/// [`included_component_ids`]: QueryFilter::included_component_ids
/// [`excluded_component_ids`]: QueryFilter::excluded_component_ids
/// [`init_state`]: QueryFilter::init_state
/// [`matches`]: QueryFilter::matches
pub trait QueryFilter {
    /// Per-archetype cached state used by [`Self::matches`].
    type State: Send + Sync;

    /// Components that filtered archetypes MUST contain.
    /// Folded into the query mask so non-matching archetypes are skipped.
    fn included_component_ids() -> Vec<ComponentId> {
        Vec::new()
    }

    /// Components that filtered archetypes must NOT contain.
    /// Archetypes containing any of these are excluded outright.
    fn excluded_component_ids() -> Vec<ComponentId> {
        Vec::new()
    }

    /// Initialize per-archetype state for row-level filtering.
    fn init_state(archetype: &mut Archetype, last_run: Tick, this_run: Tick) -> Self::State;

    /// Decide whether the row at `index` in this archetype passes the filter.
    fn matches(state: &Self::State, index: usize) -> bool;
}

// ----------------------------------------------------------------------------
// Trivial filter: ()
// ----------------------------------------------------------------------------

/// The unit type acts as a no-op filter that accepts every row.
impl QueryFilter for () {
    type State = ();

    fn init_state(_archetype: &mut Archetype, _last_run: Tick, _this_run: Tick) -> Self::State {}

    #[inline(always)]
    fn matches(_state: &Self::State, _index: usize) -> bool {
        true
    }
}

// ----------------------------------------------------------------------------
// With<T> / Without<T>
// ----------------------------------------------------------------------------

/// Archetype filter: only yield rows whose archetype contains `T`.
///
/// Useful when the query does not need to access `T`'s data (so it does not
/// appear in the [`QueryTarget`](super::QueryTarget)) but should still
/// scope the iteration.
pub struct With<T: Component>(std::marker::PhantomData<T>);

impl<T: Component> QueryFilter for With<T> {
    type State = ();

    fn included_component_ids() -> Vec<ComponentId> {
        vec![ComponentId::of::<T>()]
    }

    fn init_state(_archetype: &mut Archetype, _last_run: Tick, _this_run: Tick) -> Self::State {}

    #[inline(always)]
    fn matches(_state: &Self::State, _index: usize) -> bool {
        true
    }
}

/// Archetype filter: only yield rows whose archetype does NOT contain `T`.
pub struct Without<T: Component>(std::marker::PhantomData<T>);

impl<T: Component> QueryFilter for Without<T> {
    type State = ();

    fn excluded_component_ids() -> Vec<ComponentId> {
        vec![ComponentId::of::<T>()]
    }

    fn init_state(_archetype: &mut Archetype, _last_run: Tick, _this_run: Tick) -> Self::State {}

    #[inline(always)]
    fn matches(_state: &Self::State, _index: usize) -> bool {
        true
    }
}

// ----------------------------------------------------------------------------
// Changed<T> / Added<T>
// ----------------------------------------------------------------------------

/// Per-row state shared by `Changed<T>` and `Added<T>`: a `Send` pointer to
/// the archetype's tick vector plus the comparison window.
pub struct TickFilterState {
    ticks: SendPtr<Vec<ComponentTicks>>,
    last_run: Tick,
    this_run: Tick,
}

impl TickFilterState {
    fn new(ticks_vec: &Vec<ComponentTicks>, last_run: Tick, this_run: Tick) -> Self {
        Self {
            ticks: SendPtr::new(ticks_vec as *const Vec<ComponentTicks>),
            last_run,
            this_run,
        }
    }

    #[inline]
    unsafe fn ticks_at(&self, index: usize) -> &ComponentTicks {
        unsafe { (&*self.ticks.as_ptr()).get_unchecked(index) }
    }
}

/// Row filter: yields only entities whose `T` was mutated (or added) since
/// the system that owns this query last ran.
///
/// Implemented in terms of [`ComponentTicks::is_changed`].
pub struct Changed<T: Component>(std::marker::PhantomData<T>);

impl<T: Component> QueryFilter for Changed<T> {
    type State = TickFilterState;

    fn included_component_ids() -> Vec<ComponentId> {
        vec![ComponentId::of::<T>()]
    }

    fn init_state(archetype: &mut Archetype, last_run: Tick, this_run: Tick) -> Self::State {
        let ticks_vec = archetype
            .component_ticks
            .get(&ComponentId::of::<T>())
            .expect("Changed<T>: component_ticks vec missing - archetype not properly initialized");
        TickFilterState::new(ticks_vec, last_run, this_run)
    }

    #[inline]
    fn matches(state: &Self::State, index: usize) -> bool {
        unsafe {
            state
                .ticks_at(index)
                .is_changed(state.last_run, state.this_run)
        }
    }
}

/// Row filter: yields only entities whose `T` was added since the system
/// that owns this query last ran.
pub struct Added<T: Component>(std::marker::PhantomData<T>);

impl<T: Component> QueryFilter for Added<T> {
    type State = TickFilterState;

    fn included_component_ids() -> Vec<ComponentId> {
        vec![ComponentId::of::<T>()]
    }

    fn init_state(archetype: &mut Archetype, last_run: Tick, this_run: Tick) -> Self::State {
        let ticks_vec = archetype
            .component_ticks
            .get(&ComponentId::of::<T>())
            .expect("Added<T>: component_ticks vec missing - archetype not properly initialized");
        TickFilterState::new(ticks_vec, last_run, this_run)
    }

    #[inline]
    fn matches(state: &Self::State, index: usize) -> bool {
        unsafe {
            state
                .ticks_at(index)
                .is_added(state.last_run, state.this_run)
        }
    }
}

// ----------------------------------------------------------------------------
// Tuple filters: AND of all components
// ----------------------------------------------------------------------------

macro_rules! impl_query_filter_tuple {
    ($($T:ident),*) => {
        impl<$($T: QueryFilter),*> QueryFilter for ($($T,)*) {
            type State = ($($T::State,)*);

            fn included_component_ids() -> Vec<ComponentId> {
                let mut ids = Vec::new();
                $(ids.extend($T::included_component_ids());)*
                ids
            }

            fn excluded_component_ids() -> Vec<ComponentId> {
                let mut ids = Vec::new();
                $(ids.extend($T::excluded_component_ids());)*
                ids
            }

            #[allow(non_snake_case)]
            fn init_state(archetype: &mut Archetype, last_run: Tick, this_run: Tick) -> Self::State {
                let arch_ptr = archetype as *mut Archetype;
                unsafe { ($($T::init_state(&mut *arch_ptr, last_run, this_run),)*) }
            }

            #[allow(non_snake_case)]
            fn matches(state: &Self::State, index: usize) -> bool {
                let ($($T,)*) = state;
                $(if !$T::matches($T, index) { return false; })*
                true
            }
        }
    };
}

impl_query_filter_tuple!(A);
impl_query_filter_tuple!(A, B);
impl_query_filter_tuple!(A, B, C);
impl_query_filter_tuple!(A, B, C, D);

// ----------------------------------------------------------------------------
// Or<F>: yield rows that satisfy ANY filter in the tuple
// ----------------------------------------------------------------------------

/// Disjunction over a tuple of filters: a row matches if at least one
/// inner filter matches it.
///
/// Note: `Or` only ORs the **row-level** predicates. The archetype-level
/// included / excluded sets are intersected (intersection of inclusions,
/// union of exclusions) so that all inner filters can run safely. For
/// `Or<(Changed<A>, Changed<B>)>` this means archetypes must contain both
/// `A` and `B`, which matches Bevy's behavior.
pub struct Or<F>(std::marker::PhantomData<F>);

macro_rules! impl_query_filter_or {
    ($($T:ident),*) => {
        impl<$($T: QueryFilter),*> QueryFilter for Or<($($T,)*)> {
            type State = ($($T::State,)*);

            fn included_component_ids() -> Vec<ComponentId> {
                let mut ids = Vec::new();
                $(ids.extend($T::included_component_ids());)*
                ids
            }

            fn excluded_component_ids() -> Vec<ComponentId> {
                let mut ids = Vec::new();
                $(ids.extend($T::excluded_component_ids());)*
                ids
            }

            #[allow(non_snake_case)]
            fn init_state(archetype: &mut Archetype, last_run: Tick, this_run: Tick) -> Self::State {
                let arch_ptr = archetype as *mut Archetype;
                unsafe { ($($T::init_state(&mut *arch_ptr, last_run, this_run),)*) }
            }

            #[allow(non_snake_case)]
            fn matches(state: &Self::State, index: usize) -> bool {
                let ($($T,)*) = state;
                $(if $T::matches($T, index) { return true; })*
                false
            }
        }
    };
}

impl_query_filter_or!(A, B);
impl_query_filter_or!(A, B, C);
impl_query_filter_or!(A, B, C, D);
