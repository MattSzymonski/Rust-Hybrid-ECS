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
pub mod system;
pub mod world;

// Re-export commonly used types
pub use commands::{CommandQueue, Commands};
pub use component::Component;
pub use engine::Engine;
pub use entity::Entity;
pub use query::{GlobalComponentQuery, Query, QueryTarget};
pub use system::{State, SystemState};
pub use world::{EntityBuilder, World};
