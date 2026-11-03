//! Demonstrates parallel system execution in the Pill ECS.
//!
//! Registers four systems, turns parallel execution on (it is already the
//! engine's default), prints the scheduler's execution graph, then runs five
//! frames of the simulation.
//!
//! # Responsibilities
//!
//! - Defines the `Position`, `Velocity`, and `Health` components used by the
//!   demo.
//! - Defines four systems that move entities, decay health, apply damage, and
//!   spawn a fresh entity each frame.
//! - Runs the engine's frame loop with parallel execution enabled and prints
//!   the execution graph and per-frame output.
//!
//! # Design
//!
//! The demo builds an [`Engine`] with parallel execution on. The scheduler
//! builds its dependency graph from the components each system reads and
//! writes, and rebuilds it only when the registered systems change. Systems
//! that share no component sit in one batch and run at the same time; a
//! system that touches a component another one uses waits in a later batch.
//! Movement writes `Position` and reads `Velocity`, while health only writes
//! `Health`, so those two share the first batch.

// =============================================================================
// Imports
// =============================================================================

// Current crate
use pill_engine::*;

// =============================================================================
// Components
// =============================================================================

/// 2D position of an entity in the demo world.
///
/// Written by `movement_system` and `move_and_spawn_system`. Read by
/// `damage_system`, which checks whether the entity is within one unit of the
/// origin.
#[derive(Debug, Clone)]
struct Position {
    /// Horizontal coordinate in world units.
    x: f32,
    /// Vertical coordinate in world units.
    y: f32,
}
impl Component for Position {}

/// Velocity of an entity, added to its [`Position`] every frame.
///
/// Read by `movement_system` and `move_and_spawn_system`. Both also write
/// `Position`, so the scheduler runs them one after the other.
#[derive(Debug, Clone)]
struct Velocity {
    /// Horizontal velocity in world units per frame.
    vx: f32,
    /// Vertical velocity in world units per frame.
    vy: f32,
}
impl Component for Velocity {}

/// Hit points of an entity, decaying over time.
///
/// `health_decay_system` removes a tenth of a point every frame, and
/// `damage_system` removes one point from an entity near the origin.
#[derive(Debug, Clone)]
struct Health(f32);
impl Component for Health {}

// =============================================================================
// Systems
// =============================================================================

/// Moves every entity by its velocity and queues one fresh entity per frame.
///
/// Writes `Position` and reads `Velocity`, so it conflicts with
/// `movement_system` and runs in a later batch.
fn move_and_spawn_system(mut commands: Commands, mut query: Query<(&mut Position, &Velocity)>) {
    // Step 1: Add each entity's velocity to its position.
    for (mut position, velocity) in query.iter_mut() {
        position.x += velocity.vx;
        position.y += velocity.vy;
    }

    // Step 2: Queue a new entity at the origin with velocity (1, 1) and 50
    // health points.
    let _queued_entity = commands
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .with(Velocity { vx: 1.0, vy: 1.0 })
        .with(Health(50.0))
        .build();
}

/// Moves every entity by its velocity.
///
/// Writes `Position` and reads `Velocity`. Nothing it touches is also touched
/// by `health_decay_system`, so the two share the first batch.
fn movement_system(mut query: Query<(&mut Position, &Velocity)>) {
    for (mut position, velocity) in query.iter_mut() {
        position.x += velocity.vx;
        position.y += velocity.vy;
    }
}

/// Takes a tenth of a point of health off every entity each frame.
///
/// Writes `Health`, which no component of `movement_system` overlaps, so the
/// two share the first batch.
fn health_decay_system(mut query: Query<&mut Health>) {
    for mut health in query.iter_mut() {
        health.0 = (health.0 - 0.1).max(0.0);
    }
}

/// Takes one point of health off every entity within one unit of the origin.
///
/// Writes `Health` and reads `Position`, so it shares a component with each
/// system in the first batch and runs in the second one.
fn damage_system(mut query: Query<(&mut Health, &Position)>) {
    for (mut health, position) in query.iter_mut() {
        if position.x.abs() < 1.0 && position.y.abs() < 1.0 {
            health.0 = (health.0 - 1.0).max(0.0);
        }
    }
}

// =============================================================================
// Entry Point
// =============================================================================

/// Runs the parallel systems demo.
///
/// Builds an [`Engine`] with parallel execution enabled, registers the
/// components and systems, prints the scheduler's execution graph, then steps
/// through five frames of the simulation.
fn main() {
    // Install the engine logger, so this program's `pill_core::info!` lines
    // and the engine's own log lines both reach the terminal.
    let _ = pill_core::telemetry::TelemetryBuilder::new().init();
    pill_core::info!("=== Parallel Systems Demo ===");

    // Step 1: Create the engine. Parallel execution is already on by default;
    // the call states it for the reader.
    let mut engine = Engine::new();
    engine.set_parallel_execution(true);

    // Step 2: Register the components used by the demo.
    engine.world_mut().register_component::<Position>();
    engine.world_mut().register_component::<Velocity>();
    engine.world_mut().register_component::<Health>();

    // Step 3: Register the four systems. `movement` and `health` touch
    // different components, so they share the first batch. `damage` touches
    // both, so it runs in the second batch, and `xxx` writes `Position` too,
    // so it runs in the third.
    engine.register_system("movement", movement_system);
    engine.register_system("health", health_decay_system);
    engine.register_system("damage", damage_system);
    engine.register_system("xxx", move_and_spawn_system);

    // Step 4: Print the execution graph, which lists each batch and the
    // systems it holds.
    pill_core::info!("System Execution Graph:");
    engine.print_execution_graph();

    // Step 5: Create five entities at x = 0, 1, 2, 3 and 4, each with
    // velocity (1, 0.5) and 100 health points.
    for i in 0..5 {
        engine
            .world_mut()
            .create_entity()
            .with(Position {
                x: i as f32,
                y: 0.0,
            })
            .with(Velocity { vx: 1.0, vy: 0.5 })
            .with(Health(100.0))
            .build()
            .unwrap();
    }

    // Step 6: Run five frames, logging the frame number as each one starts.
    pill_core::info!("Running 5 frames...");
    for frame in 0..5 {
        pill_core::info!("Frame {}", frame);
        engine.process_frame().unwrap();
    }

    pill_core::info!("Demo complete!");
}
