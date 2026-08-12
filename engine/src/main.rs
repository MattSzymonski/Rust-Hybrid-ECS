//! Minimal binary demonstrating the ECS library is loaded.
//!
//! # Responsibilities
//!
//! - Prints version and entity count to confirm the library initialises correctly.
//! - Lists available examples for users to explore.
//!
//! For full examples, use `cargo run --example <name>`.

use pill_engine::Engine;

fn main() {
    let engine = Engine::new();
    println!(
        "pill_engine v{} - {} entities",
        env!("CARGO_PKG_VERSION"),
        engine.world().entity_count(),
    );
    println!("Run examples with: cargo run --example <name>");
    println!("Available: iterators_stress_test, change_detection_demo,");
    println!("           scripting_demo, parallel_systems_demo, resources_demo");
}
