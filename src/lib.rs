// ----------------------------------------------------------------------------
// Archetype-based ECS Library
// ----------------------------------------------------------------------------
//! A high-performance Entity Component System (ECS) implementation using
//! archetype-based storage for optimal cache locality and query performance.

pub mod archetype;
pub mod commands;
pub mod component;
pub mod engine;
pub mod entity;
pub mod profiling;
pub mod query;
pub mod resource;
pub mod scheduler;
pub mod scripting;
pub mod system;
pub mod world;

// ----------------------------------------------------------------------------
// Tracy ProfiledAllocator - tracks allocations in Tracy's memory view.
// Only active when the `tracing` feature is enabled.
// ----------------------------------------------------------------------------
#[cfg(feature = "tracing")]
#[global_allocator]
static ALLOC: tracy_client::ProfiledAllocator<std::alloc::System> =
    tracy_client::ProfiledAllocator::new(std::alloc::System, 100);

// Re-export commonly used types
pub use commands::{CommandError, Commands};
pub use component::{Component, ComponentId, ComponentTicks, Tick};
pub use engine::Engine;
pub use entity::Entity;
pub use query::{
    Added, BatchStats, Changed, Or, Query, QueryFilter, QueryTarget, Res, ResMut, With, Without,
};
pub use resource::{ResHandle, Resource};
pub use scheduler::{SystemAccess, SystemScheduler, TypeKey};
pub use scripting::{ScriptComponent, ScriptContext};
pub use world::{AddComponentError, BuildError, EntityBuilder, RemoveComponentError, World};
