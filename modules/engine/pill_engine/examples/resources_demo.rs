//! Demonstrates resources and component queries working together in one frame
//! loop.
//!
//! Four systems run over three entities. Two of them write a resource and one
//! reads two resources. The scheduler puts a system that writes data in a
//! different batch from every system that reads or writes that data, so the
//! batches can run in parallel.
//!
//! # Responsibilities
//!
//! - Defines two components, `Position` and `Velocity`, and two resources,
//!   `ProjectTime` (the frame step and the running total) and `Score` (total
//!   points), for a minimal project loop.
//! - Provides four systems: `time_system` (writes `ProjectTime`),
//!   `movement_system` (writes `Position`, reads `Velocity`),
//!   `scoring_system` (reads `Position`, writes `Score`), and
//!   `display_system` (reads `Position`, `ProjectTime`, and `Score`).
//! - Registers the components and systems with an `Engine`, inserts the two
//!   resources into its world, prints the execution graph, and runs a few
//!   frames with parallel execution on.
//!
//! # Design
//!
//! The scheduler compares the access patterns when the set of systems changes.
//! A write to `ProjectTime`, `Position`, or `Score` conflicts with every other
//! read or write of the same data. Two systems that conflict never share a
//! batch, and each system joins the first batch that holds nothing it conflicts
//! with.

// Current crate
use pill_engine::*;

// =============================================================================
// Components
// =============================================================================

/// Position of an entity in 2D space.
///
/// Updated every frame by `movement_system`, which adds the entity's
/// `Velocity` to it.
#[derive(Debug, Clone)]
struct Position {
    /// X coordinate.
    x: f32,
    /// Y coordinate.
    y: f32,
}
impl Component for Position {}

/// Velocity of an entity, added to its `Position` every frame.
#[derive(Debug, Clone)]
struct Velocity {
    /// Velocity along the X axis, in world units per frame.
    x: f32,
    /// Velocity along the Y axis, in world units per frame.
    y: f32,
}
impl Component for Velocity {}

// =============================================================================
// Resources
// =============================================================================

/// Frame timing for the demo loop.
///
/// Written by `time_system` and read by `display_system` every frame.
#[derive(Debug)]
struct ProjectTime {
    /// Seconds added to `elapsed_seconds` on every frame. The demo inserts it
    /// once and never changes it, so the time step stays the same.
    delta_seconds: f32,
    /// Total seconds since the loop started.
    elapsed_seconds: f32,
}
impl Resource for ProjectTime {}

/// Total points awarded so far, written by `scoring_system`.
#[derive(Debug)]
struct Score(u32);
impl Resource for Score {}

// =============================================================================
// Systems
// =============================================================================

/// Adds one frame step to the project time.
///
/// The `ResMut<ProjectTime>` parameter counts as a write of the resource, so
/// the scheduler keeps this system out of any batch that holds another system
/// reading or writing `ProjectTime`. When the resource is missing, the system
/// returns without doing anything.
fn time_system(mut time: ResMut<ProjectTime>) {
    let Some(mut project_time) = time.get_mut() else {
        return;
    };
    project_time.elapsed_seconds += project_time.delta_seconds;
}

/// Adds each entity's velocity to its position.
///
/// The query asks for `&mut Position` and `&Velocity`, so the scheduler
/// records a write of `Position` and a read of `Velocity`.
fn movement_system(mut query: Query<(&mut Position, &Velocity)>) {
    for (mut position, velocity) in query.iter_mut() {
        position.x += velocity.x;
        position.y += velocity.y;
    }
}

/// Awards points for every entity, based on its distance from the origin.
///
/// The query reads `Position`, and the `ResMut<Score>` parameter writes the
/// resource. Each distance is truncated to whole points and added to the
/// score.
fn scoring_system(mut query: Query<&Position>, mut score: ResMut<Score>) {
    let Some(mut total_score) = score.get_mut() else {
        return;
    };

    for position in query.iter_mut() {
        let distance = (position.x * position.x + position.y * position.y).sqrt();
        total_score.0 += distance as u32;
    }
}

/// Logs the frame's statistics.
///
/// The query reads `Position` together with each entity's id, and both
/// resources are read as well.
///
/// # Panics
///
/// Panics when `ProjectTime` or `Score` is missing from the world, because
/// both resource lookups unwrap.
fn display_system(
    mut query: Query<(Entity, &Position)>,
    time: Res<ProjectTime>,
    score: Res<Score>,
) {
    let project_time = time.get().unwrap();
    let total_score = score.get().unwrap();

    pill_core::info!("--- Frame stats @ {:.3}s ---", project_time.elapsed_seconds);
    pill_core::info!("  score: {:>5}", total_score.0);
    for (entity, position) in query.iter_mut() {
        pill_core::info!(
            "  entity {:?} at ({:.1}, {:.1})",
            entity,
            position.x,
            position.y
        );
    }
}

// =============================================================================
// Entry Point
// =============================================================================

/// Builds the demo world, registers the components and systems, and runs a few
/// frames to show the scheduler's batch splitting.
///
/// # Panics
///
/// Panics when an entity is built with a component type that was not
/// registered, and when `display_system` finds `ProjectTime` or `Score`
/// missing. Both resources are inserted before the first frame.
fn main() {
    // Install the engine logger, so this demo's lines and the engine's own log
    // lines are printed.
    let _ = pill_core::telemetry::TelemetryBuilder::new().init();
    pill_core::info!("=== Resources + Components Demo ===");

    // Parallel execution is already the engine's default. The call only
    // states it for the reader.
    let mut engine = Engine::new();
    engine.set_parallel_execution(true);

    // Step 1: Register the component types, then insert the starting
    // resources.
    engine.world_mut().register_component::<Position>();
    engine.world_mut().register_component::<Velocity>();
    engine.world_mut().insert_resource(ProjectTime {
        delta_seconds: 0.016,
        elapsed_seconds: 0.0,
    });
    engine.world_mut().insert_resource(Score(0));

    // Step 2: Register the systems, then print the execution graph.
    //
    // The access patterns decide the batches. `time_system` writes
    // `ProjectTime`, `movement_system` writes `Position` and reads `Velocity`,
    // `scoring_system` reads `Position` and writes `Score`, and
    // `display_system` reads all three. Every write is also read by another
    // system, so no batch holds both sides of a conflicting pair. The batches
    // come out as [time, movement], [scoring], [display].
    engine.register_system("time", time_system);
    engine.register_system("movement", movement_system);
    engine.register_system("scoring", scoring_system);
    engine.register_system("display", display_system);

    engine.print_execution_graph();

    // Step 3: Spawn three entities, each with its own starting position and
    // velocity.
    for i in 0..3 {
        engine
            .world_mut()
            .create_entity()
            .with(Position {
                x: i as f32 * 10.0,
                y: 0.0,
            })
            .with(Velocity {
                x: 1.0,
                y: (i as f32) * 0.5,
            })
            .build()
            .unwrap();
    }

    // Step 4: Run three frames, then log that the demo finished.
    for _frame in 0..3 {
        engine.process_frame().unwrap();
    }

    pill_core::info!("Demo complete!");
}
