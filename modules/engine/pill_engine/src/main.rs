//! Minimal binary demonstrating the ECS library is loaded.
//!
//! # Responsibilities
//!
//! - Logs version and entity count to confirm the library initialises correctly.
//! - Lists available examples for users to explore.
//!
//! # Design
//!
//! This binary is the default `pill_engine` package executable. It links the
//! package's library crate, constructs an [`Engine`], and logs the version
//! plus the live entity count as a smoke test of library initialisation. All
//! real workloads live in the example programs; for full examples, use
//! `cargo run --example <name>`.

// Current crate
use pill_engine::Engine;

// =============================================================================
// Entry Point
// =============================================================================

/// Runs the library smoke test and logs usage instructions.
///
/// Constructs the [`Engine`], confirms the ECS world comes up by reporting
/// the current entity count, and points the user at the example programs.
fn main() {
    // Route this program's output, and the engine's own reports, through
    // the engine logger.
    let _ = pill_core::telemetry::TelemetryBuilder::new().init();
    // Construct the engine, bringing up the ECS world.
    let engine = Engine::new();

    // Print version and entity count as an initialisation check.
    pill_core::info!(
        "pill_engine v{} - {} entities",
        env!("CARGO_PKG_VERSION"),
        engine.world().entity_count(),
    );

    // Print the example programs available for exploration.
    pill_core::info!("Run examples with: cargo run --example <name>");
    pill_core::info!("Available: iterators_stress_test, change_detection_demo,");
    pill_core::info!("           scripting_demo, parallel_systems_demo, resources_demo");
}
