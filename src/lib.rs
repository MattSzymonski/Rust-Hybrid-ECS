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
pub mod scheduler;
pub mod scripting;
pub mod system;
pub mod world;

// Re-export commonly used types
pub use commands::Commands;
pub use component::Component;
pub use engine::Engine;
pub use entity::Entity;
pub use query::{BatchStats, GlobalComponentQuery, Query, QueryTarget};
pub use scheduler::{SystemAccess, SystemScheduler};
pub use scripting::ScriptComponent;
pub use world::{AddComponentError, EntityBuilder, RemoveComponentError, World};
