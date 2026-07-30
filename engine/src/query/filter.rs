//! [`QueryFilter`] trait and built-in filter types for entity-level predicates.
//!
//! # Responsibilities
//!
//! - Defines the [`QueryFilter`] trait that layers row-level predicates on queries.
//! - Implements [`With`] / [`Without`] for archetype-level component scoping.
//! - Implements [`Changed`] / [`Added`] for per-entity change-detection filtering.
//! - Implements [`Or`] for logical-OR composition of multiple filters.
//!
//! # Design
//!
//! Filters produce (include_mask, exclude_mask) pairs that are checked against
//! each archetype's component mask at iteration start. `With<T>` adds T to the
//! include set; `Without<T>` adds T to the exclude set. `Changed<T>` and
//! `Added<T>` compare per-entity tick values against the calling system's
//! last-run baseline. `Or` produces multiple pairs - an archetype matches if
//! any pair passes.

// Standard library

// Current crate
use crate::archetype::Archetype;
use crate::component::{Component, ComponentId, ComponentTicks, Tick};

use super::ptr::SendPtr;

// =============================================================================
// QueryFilter
// =============================================================================

/// Trait for predicates that decide which entities a query yields beyond
/// the basic component-set match implied by the [`QueryTarget`].
///
/// Filters operate at two levels:
///
/// 1. Archetype-level: [`archetype_filter_pairs`] returns a list of
///    `(include, exclude)` component-id pairs. An archetype matches the
///    filter if it satisfies any pair (OR semantics). The default
///    implementation delegates to [`included_component_ids`] /
///    [`excluded_component_ids`]; [`Or`] overrides this to produce one
///    pair per inner filter.
///
/// 2. Row-level: [`init_state`] caches per-archetype data (e.g. a
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

// =============================================================================
// QueryFilter
// =============================================================================

pub trait QueryFilter {
    /// Per-archetype cached state used by [`Self::matches`].
    type State: Send + Sync;

    /// True when this filter accepts every row unconditionally (e.g. `()`).
    /// Queries use this to skip filter evaluation in the inner loop.
    /// Default: `false` - most filters do actual work.
    const ACCEPTS_ALL: bool = false;

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

    /// Returns a list of `(include_ids, exclude_ids)` pairs for archetype
    /// scoping. An archetype matches the filter if it matches any of
    /// the pairs - i.e. OR semantics across pairs.
    ///
    /// Each pair means: the archetype must contain ALL `include_ids` AND
    /// NONE of the `exclude_ids`.
    ///
    /// The default implementation returns a single pair from
    /// [`included_component_ids`] and [`excluded_component_ids`],
    /// preserving the original behaviour for simple filters.
    /// [`Or`] overrides this to return one pair per inner filter,
    /// giving correct logical-OR semantics at the archetype level.
    ///
    /// [`included_component_ids`]: QueryFilter::included_component_ids
    /// [`excluded_component_ids`]: QueryFilter::excluded_component_ids
    /// [`Or`]: Or
    fn archetype_filter_pairs() -> Vec<(Vec<ComponentId>, Vec<ComponentId>)> {
        let included = Self::included_component_ids();
        let excluded = Self::excluded_component_ids();
        if included.is_empty() && excluded.is_empty() {
            Vec::new()
        } else {
            vec![(included, excluded)]
        }
    }

    /// Initialize per-archetype state for row-level filtering.
    fn init_state(archetype: &mut Archetype, last_run: Tick, this_run: Tick) -> Self::State;

    /// Decide whether the row at `index` in this archetype passes the filter.
    fn matches(state: &Self::State, index: usize) -> bool;
}

// =============================================================================
// Trivial Filter: ()
// =============================================================================

/// The unit type acts as a no-op filter that accepts every row.
impl QueryFilter for () {
    type State = ();
    const ACCEPTS_ALL: bool = true;

    fn init_state(_archetype: &mut Archetype, _last_run: Tick, _this_run: Tick) -> Self::State {}

    #[inline(always)]
    fn matches(_state: &Self::State, _index: usize) -> bool {
        true
    }
}

// =============================================================================
// With
// =============================================================================

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
// =============================================================================
// Without
// =============================================================================

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

// =============================================================================
// Changed<T> / Added<T>
// =============================================================================

/// Per-row state shared by `Changed<T>` and `Added<T>`: a `Send` pointer to
/// the archetype's tick vector plus the comparison window.
///
/// When the component is not present in the archetype (possible when
/// `Changed<T>` appears inside an [`Or`] whose other branch matched),
/// `ticks` is `None` and all rows are rejected.
pub struct TickFilterState {
    ticks: Option<SendPtr<Vec<ComponentTicks>>>,
    last_run: Tick,
    this_run: Tick,
}

impl TickFilterState {
    fn new(ticks_vec: &Vec<ComponentTicks>, last_run: Tick, this_run: Tick) -> Self {
        Self {
            ticks: Some(SendPtr::new(ticks_vec as *const Vec<ComponentTicks>)),
            last_run,
            this_run,
        }
    }

    /// State for when the component is absent from the archetype -
    /// [`matches`](TickFilterState::matches) always returns `false`.
    fn missing() -> Self {
        Self {
            ticks: None,
            last_run: Tick(0),
            this_run: Tick(0),
        }
    }

    /// Returns `true` if the component exists in this archetype.
    #[inline]
    fn is_present(&self) -> bool {
        self.ticks.is_some()
    }

    /// # Safety
    /// Caller must ensure `is_present()` returned `true` before calling.
    #[inline]
    unsafe fn ticks_at(&self, index: usize) -> &ComponentTicks {
        debug_assert!(self.is_present(), "ticks_at called on missing component");
        unsafe { (&*self.ticks.as_ref().unwrap_unchecked().as_ptr()).get_unchecked(index) }
    }
}

// =============================================================================
// Changed
// =============================================================================

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
        match archetype.component_ticks.get(&ComponentId::of::<T>()) {
            Some(ticks_vec) => TickFilterState::new(ticks_vec, last_run, this_run),
            // Component not in this archetype - possible when Changed<T>
            // appears inside an Or whose other branch matched the archetype.
            None => TickFilterState::missing(),
        }
    }

    #[inline]
    fn matches(state: &Self::State, index: usize) -> bool {
        if !state.is_present() {
            return false;
        }
        unsafe {
            state
                .ticks_at(index)
                .is_changed(state.last_run, state.this_run)
        }
    }
}

// =============================================================================
// Added
// =============================================================================

/// Row filter: yields only entities whose `T` was added since the system
/// that owns this query last ran.
pub struct Added<T: Component>(std::marker::PhantomData<T>);

impl<T: Component> QueryFilter for Added<T> {
    type State = TickFilterState;

    fn included_component_ids() -> Vec<ComponentId> {
        vec![ComponentId::of::<T>()]
    }

    fn init_state(archetype: &mut Archetype, last_run: Tick, this_run: Tick) -> Self::State {
        match archetype.component_ticks.get(&ComponentId::of::<T>()) {
            Some(ticks_vec) => TickFilterState::new(ticks_vec, last_run, this_run),
            None => TickFilterState::missing(),
        }
    }

    #[inline]
    fn matches(state: &Self::State, index: usize) -> bool {
        if !state.is_present() {
            return false;
        }
        unsafe {
            state
                .ticks_at(index)
                .is_added(state.last_run, state.this_run)
        }
    }
}

// =============================================================================
// Tuple Filters (AND)
// =============================================================================

/// Compute the cross-product of filter pairs for AND semantics.
///
/// Given a list of filter-pair lists (one per conjunct), returns all
/// combinations where each conjunct contributes one of its pairs.
/// For `(FilterA, FilterB)` this is the Cartesian product of A's pairs
/// and B's pairs, with includes/excludes merged within each combination.
///
/// If any conjunct has zero pairs (meaning "no archetype restrictions"),
/// it is skipped (does not restrict the result).
fn and_filter_pairs(
    all_pairs: &[Vec<(Vec<ComponentId>, Vec<ComponentId>)>],
) -> Vec<(Vec<ComponentId>, Vec<ComponentId>)> {
    // Start with one empty pair (no restrictions).
    let mut acc: Vec<(Vec<ComponentId>, Vec<ComponentId>)> = vec![(Vec::new(), Vec::new())];

    for inner_pairs in all_pairs {
        if inner_pairs.is_empty() {
            // No archetype restrictions from this conjunct - skip.
            continue;
        }
        let mut next = Vec::with_capacity(acc.len() * inner_pairs.len());
        for (included, excluded) in &acc {
            for (inner_included, inner_excluded) in inner_pairs {
                let mut merged_included = included.clone();
                let mut merged_excluded = excluded.clone();
                merged_included.extend_from_slice(inner_included);
                merged_excluded.extend_from_slice(inner_excluded);
                next.push((merged_included, merged_excluded));
            }
        }
        acc = next;
    }
    acc
}

macro_rules! impl_query_filter_tuple {
    ($($T:ident),*) => {
        impl<$($T: QueryFilter),*> QueryFilter for ($($T,)*) {
            type State = ($($T::State,)*);

            fn included_component_ids() -> Vec<ComponentId> {
                let mut ids = Vec::with_capacity(crate::config::QueryConfig::DEFAULT_TUPLE_COMPONENT_IDS_CAPACITY);
                $(ids.extend($T::included_component_ids());)*
                ids
            }

            fn excluded_component_ids() -> Vec<ComponentId> {
                let mut ids = Vec::with_capacity(crate::config::QueryConfig::DEFAULT_TUPLE_COMPONENT_IDS_CAPACITY);
                $(ids.extend($T::excluded_component_ids());)*
                ids
            }

            /// AND semantics: the cross-product of inner filter pairs.
            /// For `(Or<(With<A>, With<B>)>, Without<C>)` this yields
            /// `[({A},{C}), ({B},{C})]` - (has A AND lacks C) OR (has B AND lacks C).
            fn archetype_filter_pairs() -> Vec<(Vec<ComponentId>, Vec<ComponentId>)> {
                let all_pairs = [$($T::archetype_filter_pairs()),*];
                and_filter_pairs(&all_pairs)
            }

            #[allow(non_snake_case)]
            fn init_state(archetype: &mut Archetype, last_run: Tick, this_run: Tick) -> Self::State {
                let archetype_ptr = archetype as *mut Archetype;
                unsafe { ($($T::init_state(&mut *archetype_ptr, last_run, this_run),)*) }
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

// =============================================================================
// Or<F> (Disjunction)
// =============================================================================

/// Disjunction over a tuple of filters: a row matches if at least one
/// inner filter matches it.
///
/// `Or` correctly implements logical-OR at both the archetype level
/// (via [`archetype_filter_pairs`]) and the row level (via [`matches`]).
/// For `Or<(With<A>, With<B>)>`, an archetype containing `A`, `B`, or
/// both will be included - matching the expected OR semantics.
///
/// [`archetype_filter_pairs`]: QueryFilter::archetype_filter_pairs
/// [`matches`]: QueryFilter::matches

// =============================================================================
// Or
// =============================================================================

pub struct Or<F>(std::marker::PhantomData<F>);

macro_rules! impl_query_filter_or {
    ($($T:ident),*) => {
        impl<$($T: QueryFilter),*> QueryFilter for Or<($($T,)*)> {
            type State = ($($T::State,)*);

            fn included_component_ids() -> Vec<ComponentId> {
                let mut ids = Vec::with_capacity(crate::config::QueryConfig::DEFAULT_TUPLE_COMPONENT_IDS_CAPACITY);
                $(ids.extend($T::included_component_ids());)*
                ids
            }

            fn excluded_component_ids() -> Vec<ComponentId> {
                let mut ids = Vec::with_capacity(crate::config::QueryConfig::DEFAULT_TUPLE_COMPONENT_IDS_CAPACITY);
                $(ids.extend($T::excluded_component_ids());)*
                ids
            }

            /// Returns the union of all inner filter pairs: an archetype
            /// matches the `Or` if it matches ANY inner filter.
            ///
            /// If any inner filter returns zero pairs (meaning "no archetype
            /// restrictions - matches everything"), the whole `Or` also
            /// returns zero pairs. OR with "always true" is "always true".
            fn archetype_filter_pairs() -> Vec<(Vec<ComponentId>, Vec<ComponentId>)> {
                let mut pairs = Vec::with_capacity(crate::config::QueryConfig::DEFAULT_FILTER_PAIRS_CAPACITY);
                $(
                    let inner = $T::archetype_filter_pairs();
                    if inner.is_empty() {
                        // One branch has no restrictions - the whole Or
                        // matches every archetype.
                        return Vec::new();
                    }
                    pairs.extend(inner);
                )*
                pairs
            }

            #[allow(non_snake_case)]
            fn init_state(archetype: &mut Archetype, last_run: Tick, this_run: Tick) -> Self::State {
                let archetype_ptr = archetype as *mut Archetype;
                unsafe { ($($T::init_state(&mut *archetype_ptr, last_run, this_run),)*) }
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
