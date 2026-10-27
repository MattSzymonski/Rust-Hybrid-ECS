//! Minimal binary confirming the ECS library loads.
//!
//! # Responsibilities
//!
//! - Installs telemetry and constructs an [`Engine`].
//! - Logs the crate version and the live entity count as an initialisation
//!   check.
//! - Points the reader at the example programs.
//!
//! # Design
//!
//! This is the `pill_engine` package's binary target. It links the package's
//! library crate and does nothing beyond bringing the ECS world up, so a
//! library that fails to initialise fails visibly here. Real workloads live in
//! the example programs (`cargo run --example <name>`).

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

    // Name the example programs the package ships.
    pill_core::info!("Run examples with: cargo run --example <name>");
    pill_core::info!("Available: iterators_stress_test, change_detection_demo,");
    pill_core::info!("           scripting_demo, parallel_systems_demo, resources_demo");
}
