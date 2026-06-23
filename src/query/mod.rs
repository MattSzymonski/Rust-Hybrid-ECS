//! # Query System - Component Access and Iteration
//!
//! Queries provide efficient iteration over entities with specific components.
//! The query system uses the [`QueryTarget`] trait to support flexible component
//! access patterns (immutable / mutable / `Entity`) and the [`QueryFilter`]
//! trait to layer additional predicates on top, including change detection.
//!
//! ## How it works
//! - The `Query` struct is parameterized by a [`QueryTarget`] (the data
//!   yielded per row) and a [`QueryFilter`] (an optional predicate).
//! - Component requirements from the target and filter are folded into a
//!   single [`ComponentMask`] used to skip non-matching archetypes.
//! - For parallel iteration, per-archetype state caches raw pointers so
//!   worker threads access components without repeated lookups.
//! - [`BatchStats`] reports how Rayon distributed work across threads.
//!
//! ## Module layout
//!
//! - [`target`] - the [`QueryTarget`] trait and its impls (`Entity`, `&T`,
//!   `&mut T`, tuples)
//! - [`filter`] - the [`QueryFilter`] trait and concrete filters
//!   ([`With`], [`Without`], [`Changed`], [`Added`], [`Or`])
//! - [`iter`] - the sequential and parallel iterator types
//! - [`ptr`] - thread-safe raw-pointer wrappers shared by target and filter
//! - [`resource`] - [`Res`] / [`ResMut`] system parameters
//!
//! ## Usage Examples
//!
//! ```no_run
//! # use ecs_hybrid::*;
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

pub(crate) mod change_detection;
mod filter;
mod iter;
mod ptr;
#[allow(clippy::module_inception)]
mod query;
mod resource;
mod target;

#[cfg(test)]
mod tests;

use crate::archetype::ArchetypeId;

pub use filter::{Added, Changed, Or, QueryFilter, With, Without};
pub use iter::{BatchStats, ParForEachResult, ParQueryIter, QueryIterMut};
pub use query::Query;
pub use resource::{Res, ResMut};
pub use target::QueryTarget;

/// Cached archetype state for filtered parallel iteration:
/// `(archetype_id, target_state, filter_state, entity_count)`.
pub(crate) type FilteredArchetypeRange<QS, FS> = (ArchetypeId, QS, FS, usize);
