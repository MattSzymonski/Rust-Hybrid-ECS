//! Hot-reloadable project module for migration integration testing.
//!
//! # Responsibilities
//!
//! - Defines three persistable components used by migration tests.
//! - Implements a single `counter_system` that prints a timestamp at threshold.
//! - Exports `project_init` and `project_update` for the standalone host.
//!
//! # Design
//!
//! This crate is compiled as a `cdylib` (dynamic library). The standalone
//! host loads it at runtime and calls `project_init` to register the component
//! and system. When source files change, the host rebuilds and reloads this
//! module without restarting. Component data is preserved across reloads
//! via JSON serialization and matched by type name.
//!
//! Components are declared with `#[derive(PillComponent)]`, which registers
//! them at init automatically; `#[pill_project]` generates the `project_*`
//! entry points from the `init` function below.

// External crates
use serde::{Deserialize, Serialize};

// Current crate
use pill_engine::*;

// =============================================================================
// Components
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default, PillComponent)]
#[pill(persistable)]
struct FrameCounter {
    count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PillComponent)]
#[pill(persistable)]
struct SpatialPosition {
    horizontal: f32,
    vertical: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PillComponent)]
#[pill(persistable)]
struct LinearVelocity {
    horizontal_speed: f32,
    vertical_speed: f32,
}

// =============================================================================
// Systems
// =============================================================================

/// Increments the counter every frame. When it reaches the threshold,
/// resets and prints a timestamp to the console.
fn counter_system(mut query: Query<&mut FrameCounter>) {
    const THRESHOLD: u64 = 200;

    for mut counter in query.iter_mut() {
        counter.count += 1;
        if counter.count >= THRESHOLD {
            counter.count = 0;

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let seconds = now.as_secs();
            let milliseconds = now.subsec_millis();

            let hours = (seconds / 3600) % 24;
            let minutes = (seconds / 60) % 60;
            let secs = seconds % 60;

            println!(
                "counter tick [{:02}:{:02}:{:02}.{:03}]",
                hours, minutes, secs, milliseconds
            );
        }
    }
}

// =============================================================================
// FFI Entry Points
// =============================================================================

/// Registers systems and seed entities; component registration happens
/// automatically from the `PillComponent` derives before this runs.
#[pill_project]
fn init(engine: &mut Engine) -> u32 {
    engine.register_system("counter", counter_system);

    // Seed multiple archetypes so migration tests can validate per-component behavior.
    let _ = engine
        .world_mut()
        .create_entity()
        .with(FrameCounter { count: 0 })
        .build();

    let _ = engine
        .world_mut()
        .create_entity()
        .with(FrameCounter { count: 90 })
        .with(SpatialPosition {
            horizontal: 10.0,
            vertical: 20.0,
        })
        .build();

    let _ = engine
        .world_mut()
        .create_entity()
        .with(SpatialPosition {
            horizontal: 1.0,
            vertical: 2.0,
        })
        .with(LinearVelocity {
            horizontal_speed: 1.5,
            vertical_speed: 0.25,
        })
        .build();

    let _ = engine
        .world_mut()
        .create_entity()
        .with(FrameCounter { count: 180 })
        .with(SpatialPosition {
            horizontal: -5.0,
            vertical: 8.0,
        })
        .with(LinearVelocity {
            horizontal_speed: 0.5,
            vertical_speed: 0.75,
        })
        .build();

    // Report successful registration so the host keeps this generation.
    0
}
