// ============================================================================
// Archetype-based ECS Library
// ============================================================================
//! A high-performance Entity Component System (ECS) implementation using
//! archetype-based storage for optimal cache locality and query performance.

pub mod archetype;
pub mod commands;
pub mod component;
pub mod engine;
pub mod entity;
pub mod query;
pub mod resource;
pub mod scheduler;
pub mod scripting;
pub mod system;
pub mod world;

// Re-export commonly used types
pub use commands::{CommandError, Commands};
pub use component::{Component, ComponentTicks, Tick};
pub use engine::Engine;
pub use entity::Entity;
pub use query::{
    Added, BatchStats, Changed, Or, Query, QueryFilter, QueryTarget, Res, ResMut, With, Without,
};
pub use resource::{ResHandle, Resource};
pub use scheduler::{SystemAccess, SystemScheduler};
pub use scripting::{ScriptComponent, ScriptContext};
pub use world::{AddComponentError, BuildError, EntityBuilder, RemoveComponentError, World};
