// ----------------------------------------------------------------------------
// Archetype-based ECS - Library binary
// ----------------------------------------------------------------------------
// This is a minimal binary that demonstrates the library is loaded.
// For full examples, use: cargo run --example <name>
//   e.g. cargo run --example iterators_stress_test

use ecs_hybrid::Engine;

fn main() {
    let engine = Engine::new();
    println!(
        "ecs_hybrid v{} - {} entities",
        env!("CARGO_PKG_VERSION"),
        engine.world().entity_count(),
    );
    println!("Run examples with: cargo run --example <name>");
    println!("Available: iterators_stress_test, change_detection_demo,");
    println!("           scripting_demo, parallel_systems_demo, resources_demo");
}
