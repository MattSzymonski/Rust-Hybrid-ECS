//! Hot-reloadable project module for migration integration testing.
//!
//! # Responsibilities
//!
//! - Defines four persistable components used by migration tests.
//! - Implements a single `counter_system` that prints a timestamp at threshold.
//! - Prints per-generation value witnesses for the components migration must
//!   preserve, so tests can assert values, not just entity counts.
//! - Exports `pill_module_init` for the standalone host.
//!
//! # Design
//!
//! This crate is compiled as a `cdylib` (dynamic library). The standalone
//! host loads it at runtime and calls `pill_module_init` to register the component
//! and system. When source files change, the host rebuilds and reloads this
//! module without restarting. Component data is preserved across reloads
//! via JSON serialization and matched by type name.
//!
//! Components are declared with `#[derive(PillComponent)]`, which registers
//! them at init automatically; `#[pill_project]` generates the `project_*`
//! entry points from the `init` function below.
//!
//! The witness systems print once per module generation (their guard statics
//! are fresh in every reloaded module) and run after migration has settled,
//! which is exactly when a migration test needs to read the preserved values.

// Standard library
use std::sync::atomic::{AtomicBool, Ordering};

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

/// A persistable component whose fields own heap data, so migration has to
/// round-trip a `Vec`, a `String`, and an engine-owned `DynamicBuffer` through
/// JSON in both directions.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PillComponent)]
#[pill(persistable)]
struct MarkerTrail {
    points: Vec<f32>,
    label: String,
    samples: DynamicBuffer<f32>,
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
// Value witnesses
// =============================================================================

/// Guards the once-per-generation `SpatialPosition` witness print.
static POSITION_WITNESS_PRINTED: AtomicBool = AtomicBool::new(false);

/// Prints every `SpatialPosition` value once per module generation.
fn position_witness_system(mut query: Query<&SpatialPosition>) {
    if POSITION_WITNESS_PRINTED.swap(true, Ordering::Relaxed) {
        return;
    }
    for position in query.iter_mut() {
        println!(
            "[project] witness SpatialPosition({:.2},{:.2})",
            position.horizontal, position.vertical
        );
    }
}

/// Guards the once-per-generation `MarkerTrail` witness print.
static TRAIL_WITNESS_PRINTED: AtomicBool = AtomicBool::new(false);

/// Prints every `MarkerTrail` value once per module generation, including the
/// heap contents migration must carry across (element count, sum, text, and
/// the engine-owned buffer's length and last element).
fn trail_witness_system(mut query: Query<&MarkerTrail>) {
    if TRAIL_WITNESS_PRINTED.swap(true, Ordering::Relaxed) {
        return;
    }
    for trail in query.iter_mut() {
        let sum: f32 = trail.points.iter().sum();
        let last = trail.samples.last().copied().unwrap_or(0.0);
        println!(
            "[project] witness MarkerTrail(points={},sum={:.2},label=\"{}\",samples={},last={:.2})",
            trail.points.len(),
            sum,
            trail.label,
            trail.samples.len(),
            last,
        );
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
    engine.register_system("position_witness", position_witness_system);
    engine.register_system("trail_witness", trail_witness_system);

    // Seed multiple archetypes so migration tests can validate per-component behavior.
    let _ = engine
        .world_mut()
        .create_entity()
        .with(FrameCounter { count: 0 })
        .with(MarkerTrail {
            points: vec![1.0, 2.0, 3.0],
            label: String::from("alpha"),
            samples: DynamicBuffer::from_slice(&[0.5, 1.5]),
        })
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
        .with(MarkerTrail {
            points: vec![4.0, 5.0],
            label: String::from("beta"),
            samples: DynamicBuffer::from_slice(&[2.5]),
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
