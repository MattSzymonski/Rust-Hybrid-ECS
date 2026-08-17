//! Demonstrates parallel system execution in the Pill ECS.
//!
//! Registers four systems, enables parallel execution on the engine, prints
//! the scheduler's execution graph, then runs five frames of the simulation.
//!
//! # Responsibilities
//!
//! - Defines the `Position`, `Velocity`, and `Health` components used by the
//!   demo and makes them accessible through trait objects.
//! - Defines systems that move entities, decay and apply damage to health,
//!   and spawn a fresh entity each frame.
//! - Runs the engine's frame loop with parallel execution enabled and prints
//!   the execution graph and per-frame output.
//!
//! # Design
//!
//! The demo builds an [`Engine`] with parallel execution enabled. The
//! scheduler derives the dependency graph from each system's read/write
//! access to components, so systems without conflicting access (movement
//! writes `Position` and reads `Velocity`, while health writes `Health`) run
//! in parallel, and conflicting systems are serialised automatically.

// =============================================================================
// Imports
// =============================================================================

// External crates
use trait_type_map::impl_trait_accessible;

// Current crate
use pill_engine::*;

// =============================================================================
// Components
// =============================================================================

/// 2D position of an entity in the demo world.
///
/// Moved by `movement_system` and read by `damage_system` to decide whether
/// an entity is close enough to the origin to take damage.
#[derive(Debug, Clone)]
struct Position {
    /// Horizontal coordinate in world units.
    x: f32,
    /// Vertical coordinate in world units.
    y: f32,
}
impl Component for Position {}

/// Velocity of an entity, applied to its [`Position`] every frame.
///
/// Read by `movement_system` and `ttt_system` to displace entities each frame.
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
/// Decayed by `health_system` each frame and reduced by `damage_system` for
/// entities near the origin.
#[derive(Debug, Clone)]
struct Health(f32);
impl Component for Health {}

// Make components accessible through trait objects
impl_trait_accessible!(dyn Component; Position, Velocity, Health);

// =============================================================================
// Systems
// =============================================================================

/// Moves every entity by its velocity and spawns one fresh entity per frame.
///
/// Writes `Position` and reads `Velocity`, which conflicts with
/// `movement_system`; the scheduler serialises the two systems.
fn ttt_system(mut commands: Commands, mut query: Query<(&mut Position, &Velocity)>) {
    // Step 1: Displace every entity by its velocity.
    for (mut pos, vel) in query.iter_mut() {
        pos.x += vel.vx;
        pos.y += vel.vy;
    }

    // Step 2: Spawn a fresh entity at the origin with a starting velocity.
    let _x = commands
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .with(Velocity { vx: 1.0, vy: 1.0 })
        .with(Health(50.0))
        .build();
}

/// Moves every entity by its velocity.
///
/// Writes `Position` and reads `Velocity`; runs in parallel with systems that
/// do not touch those components.
fn movement_system(mut query: Query<(&mut Position, &Velocity)>) {
    for (mut pos, vel) in query.iter_mut() {
        pos.x += vel.vx;
        pos.y += vel.vy;
    }
}

/// Decays every entity's health by a small amount each frame.
///
/// Writes `Health`; runs in parallel with systems that do not touch `Health`.
fn health_system(mut query: Query<&mut Health>) {
    for mut health in query.iter_mut() {
        health.0 = (health.0 - 0.1).max(0.0);
    }
}

/// Applies damage to entities whose position is within one unit of the origin.
///
/// Writes `Health` and reads `Position`.
fn damage_system(mut query: Query<(&mut Health, &Position)>) {
    for (mut health, pos) in query.iter_mut() {
        if pos.x.abs() < 1.0 && pos.y.abs() < 1.0 {
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
    println!("=== Parallel Systems Demo ===\n");

    // Step 1: Create the engine with parallel execution enabled.
    let mut engine = Engine::new();
    engine.set_parallel_execution(true);

    // Step 2: Register the components used by the demo.
    engine.world_mut().register_component::<Position>();
    engine.world_mut().register_component::<Velocity>();
    engine.world_mut().register_component::<Health>();

    // Step 3: Register systems - these can run in parallel!
    // movement_system: writes Position, reads Velocity
    // health_system: writes Health
    // No conflicts! Can run in parallel.
    engine.register_system("movement", movement_system);
    engine.register_system("health", health_system);
    engine.register_system("damage", damage_system);
    engine.register_system("xxx", ttt_system);

    // Step 4: Print the execution graph built by the scheduler.
    println!("System Execution Graph:");
    engine.print_execution_graph();
    println!();

    // Step 5: Create entities spread out along the x-axis.
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

    // Step 6: Run five frames of the simulation.
    println!("Running 5 frames...\n");
    for frame in 0..5 {
        println!("Frame {}", frame);
        engine.process_frame().unwrap();
    }

    println!("\nDemo complete!");
}
