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
//! The query is parameterized by a target (what data to fetch) and a filter
//! (which entities to skip). Both fold their component requirements into a
//! single [`ComponentMask`] used to skip non-matching archetypes via bitwise
//! AND. For parallel iteration, per-archetype state caches raw pointers so
//! worker threads access components without repeated lookups.
//!
//! ## Module layout
//!
//! - [`target`] - the [`QueryTarget`] trait and its impls
//! - [`filter`] - the [`QueryFilter`] trait and concrete filters
//! - [`iter`] - sequential and parallel iterator types
//! - [`ptr`] - thread-safe raw-pointer wrappers
//! - [`resource`] - [`Res`] / [`ResMut`] system parameters
//! - [`change_detection`] - [`Mut`] smart pointer for tick tracking
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
pub(crate) mod ptr;
#[allow(clippy::module_inception)]
mod query;
mod resource;
mod target;

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests;

// Current crate
use crate::archetype::ArchetypeId;

pub use filter::{Added, Changed, Or, QueryFilter, With, Without};
pub use iter::{BatchStats, ParForEachResult, ParQueryIter, QueryIterMut};
pub use query::Query;
pub use resource::{Res, ResMut};
pub use target::QueryTarget;

/// Cached archetype state for filtered parallel iteration:
/// `(archetype_id, target_state, filter_state, entity_count)`.
pub(crate) type FilteredArchetypeRange<QS, FS> = (ArchetypeId, QS, FS, usize);
