//! The migration witnesses: four persistable components, the entities that
//! carry them and the systems that print their values once per generation.
//!
//! # Responsibilities
//!
//! - Declare the components the migration scenarios rewrite: plain scalars,
//!   and one whose fields own heap data, so a schema change has to carry a
//!   `Vec`, a `String` and an engine-owned `DynamicBuffer` through JSON.
//! - Seed one entity per archetype, so every component has data to witness.
//! - Print each witness once per generation ([`counter_system`],
//!   [`position_witness_system`], [`trail_witness_system`]).
//!
//! # Design
//!
//! The witness guards are `static` atomics, and a reload gives every new
//! generation its own statics, so each reload prints once and only once -
//! which is exactly when a migration test needs to read the preserved values.
//!
//! Seeding runs on every generation, deliberately: the migration scenarios
//! read cumulative entity counts, so a reload that skipped it would hide a
//! migration that dropped rows. The source text of the components, of the
//! seeds and of the witnesses is the anchor `test_hot_reload_migration.py`
//! edits, which is why those declarations carry no per-field documentation.

// Standard library
use std::sync::atomic::{AtomicBool, Ordering};

// External crates
use pill_engine::{DynamicBuffer, Engine, PillComponent, Query, World};
use serde::{Deserialize, Serialize};

/// A per-frame counter, the component the counter system advances.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PillComponent)]
#[pill(persistable)]
struct FrameCounter {
    count: u64,
}

/// A two-axis position, the simplest shape a schema change can widen.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PillComponent)]
#[pill(persistable)]
struct SpatialPosition {
    horizontal: f32,
    vertical: f32,
}

/// A two-axis velocity, paired with [`SpatialPosition`] on one archetype.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PillComponent)]
#[pill(persistable)]
struct LinearVelocity {
    horizontal_speed: f32,
    vertical_speed: f32,
}

/// A component whose fields own heap data, so migration has to round-trip a
/// `Vec`, a `String` and an engine-owned `DynamicBuffer` through JSON in both
/// directions.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PillComponent)]
#[pill(persistable)]
struct MarkerTrail {
    points: Vec<f32>,
    label: String,
    samples: DynamicBuffer<f32>,
}

/// Guards the once-per-generation [`SpatialPosition`] witness print.
static POSITION_WITNESS_PRINTED: AtomicBool = AtomicBool::new(false);

/// Guards the once-per-generation [`MarkerTrail`] witness print.
static TRAIL_WITNESS_PRINTED: AtomicBool = AtomicBool::new(false);

/// Registers the witness systems and seeds the entities they read.
pub(crate) fn register(engine: &mut Engine) {
    engine.register_system("counter", counter_system);
    engine.register_system("position_witness", position_witness_system);
    engine.register_system("trail_witness", trail_witness_system);
    seed(engine.world_mut());
}

/// Seeds one entity per archetype, on every generation.
fn seed(world: &mut World) {
    let _ = world
        .create_entity()
        .with(FrameCounter { count: 0 })
        .with(MarkerTrail {
            points: vec![1.0, 2.0, 3.0],
            label: String::from("alpha"),
            samples: DynamicBuffer::from_slice(&[0.5, 1.5]),
        })
        .build();

    let _ = world
        .create_entity()
        .with(FrameCounter { count: 90 })
        .with(SpatialPosition {
            horizontal: 10.0,
            vertical: 20.0,
        })
        .build();

    let _ = world
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

    let _ = world
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
}

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
