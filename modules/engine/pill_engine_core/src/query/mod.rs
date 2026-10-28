//! Query system - efficient iteration over entities with specific components.
//!
//! # Responsibilities
//!
//! - Defines the [`Query`] type as the primary entry point for component iteration.
//! - Provides [`QueryTarget`] for data shapes (`&T`, `&mut T`, `Entity`, tuples).
//! - Provides [`QueryFilter`] for predicates (`With`, `Without`, `Changed`, `Added`, `Or`).
//! - Implements sequential ([`QueryIterMut`]) and parallel ([`ParQueryIter`]) iterators.
//! - Re-exports [`Res`] / [`ResMut`] for resource access in systems.
//!
//! # Design
//!
//! A query is parameterized by a target (what data to fetch) and a filter
//! (which entities to skip). The target folds its component ids into one
//! [`ComponentMask`](crate::component::ComponentMask); the filter contributes
//! `(include, exclude)` mask pairs that are OR-ed, and an archetype is skipped
//! unless the target mask and some filter pair both pass their bitwise AND.
//! Both levels resolve archetype storage once and hand worker threads raw
//! pointers, so parallel iteration never repeats a lookup.
//!
//! [`query`] holds [`Query`] itself; the rest are [`target`], [`filter`],
//! [`iter`], [`ptr`] (thread-safe raw-pointer wrappers), [`resource`] ([`Res`] /
//! [`ResMut`]) and [`change_detection`] (the `Mut` smart pointer for tick
//! tracking).
//!
//! # Examples
//!
//! ```no_run
//! # use pill_engine::*;
//! # #[derive(Debug, Clone)] struct Transform { x: f32, y: f32 }
//! # impl Component for Transform {}
//! # #[derive(Debug, Clone)] struct Velocity { x: f32, y: f32 }
//! # impl Component for Velocity {}
//! // Sequential iteration
//! fn movement_system(mut query: Query<(&mut Transform, &Velocity)>) {
//!     for (mut transform, velocity) in query.iter_mut() {
//!         transform.x += velocity.x * 0.016;
//!     }
//! }
//!
//! // Parallel iteration
//! fn physics_system(mut query: Query<(&mut Transform, &Velocity)>) {
//!     query.par_iter_mut().for_each(|(mut transform, velocity)| {
//!         transform.x += velocity.x * 0.016;
//!     });
//! }
//!
//! // Filter on change detection
//! fn react(mut q: Query<(Entity, &Transform), Changed<Transform>>) {
//!     for (entity, t) in q.iter_mut() { /* ... */ }
//! }
//! ```

/// [`Mut`](change_detection::Mut) smart pointer with tick-based change
/// detection for components.
pub(crate) mod change_detection;
mod filter;
mod iter;

/// Thread-safe raw-pointer wrappers shared by query targets and filters.
pub(crate) mod ptr;
#[allow(clippy::module_inception)]
mod query;
mod resource;
mod target;

// Current crate

// =============================================================================
// Re-exports
// =============================================================================

// Filter predicates and the query filter trait.
pub use filter::{Added, Changed, Or, QueryFilter, With, Without};

// Sequential and parallel query iterators.
pub use iter::{BatchStats, ParQueryIter, QueryIterMut};

// Core query type and resource accessors.
pub use query::Query;
pub use resource::{Res, ResMut};
pub use target::QueryTarget;

// =============================================================================
// Types
// =============================================================================

/// Cached per-archetype state for parallel iteration.
///
/// One record per matching archetype, built once per `par_iter_mut` call: the
/// storage pointers the target and the filter resolved, and the row count they
/// were resolved against.
pub(crate) struct FilteredArchetypeRange<TargetState, FilterState> {
    /// Cached storage pointers for the queried components.
    pub(crate) target_state: TargetState,
    /// Cached state the row filter needs for this archetype.
    pub(crate) filter_state: FilterState,
    /// Rows in the archetype when the states were built.
    pub(crate) entity_count: usize,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests;
