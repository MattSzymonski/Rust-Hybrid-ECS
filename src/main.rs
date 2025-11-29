// ============================================================================
// Archetype-based ECS - Stress Test
// ============================================================================
//! Main entry point that runs the stress test directly.

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

// Include example modules
mod examples {
    pub mod stress_test;
}

fn main() {
    examples::stress_test::main();
}
