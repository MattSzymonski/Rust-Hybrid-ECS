//! Minimal binary demonstrating the ECS library is loaded.
//!
//! # Responsibilities
//!
//! - Prints version and entity count to confirm the library initialises correctly.
//! - Lists available examples for users to explore.
//!
//! # Design
//!
//! This binary is the default `pill_engine` package executable. It links the
//! package's library crate, constructs an [`Engine`], and prints the version
//! plus the live entity count as a smoke test of library initialisation. All
//! real workloads live in the example programs; for full examples, use
//! `cargo run --example <name>`.

// Current crate
use pill_engine::Engine;

// =============================================================================
// Entry Point
// =============================================================================

/// Runs the library smoke test and prints usage instructions.
///
/// Constructs the [`Engine`], confirms the ECS world comes up by reporting
/// the current entity count, and points the user at the example programs.
fn main() {
    // Construct the engine, bringing up the ECS world.
    let engine = Engine::new();

    // Print version and entity count as an initialisation check.
    println!(
        "pill_engine v{} - {} entities",
        env!("CARGO_PKG_VERSION"),
        engine.world().entity_count(),
    );

    // Print the example programs available for exploration.
    println!("Run examples with: cargo run --example <name>");
    println!("Available: iterators_stress_test, change_detection_demo,");
    println!("           scripting_demo, parallel_systems_demo, resources_demo");
}
