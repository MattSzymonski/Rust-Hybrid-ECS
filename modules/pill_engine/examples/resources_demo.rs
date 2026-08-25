//! Demo: Resources + Components + Systems Together.
//!
//! Demonstrates using resources (`Res`/`ResMut`) alongside component queries
//! in a single frame loop. The scheduler respects resource access patterns
//! when building the dependency graph for parallel execution.
//!
//! # Responsibilities
//!
//! - Defines two components, `Position` and `Velocity`, and two resources,
//!   `ProjectTime` (delta time, elapsed) and `Score` (total points), for a
//!   minimal project loop.
//! - Provides four systems: `time_system` (writes `ProjectTime`),
//!   `movement_system` (writes `Position`, reads `Velocity`),
//!   `scoring_system` (reads `Position`, writes `Score`), and
//!   `display_system` (reads `Position`, `ProjectTime`, and `Score`).
//! - Registers components, resources, and systems with an `Engine`, prints the
//!   execution graph, and runs a few frames under parallel execution.
//!
//! # Design
//!
//! The scheduler analyses each system's access pattern *before* any frame and
//! splits batches wherever access patterns conflict: writes to `ProjectTime`,
//! `Position`, and `Score` force a batch split against readers of the same
//! storage. This demo is the smallest end-to-end illustration of that
//! resource-aware dependency graph.

// External crates
use pill_engine::*;
use trait_type_map::impl_trait_accessible;

// ============================================================================
// Components
// ============================================================================

/// Position of an entity in 2D space.
///
/// Updated each frame by `movement_system` based on the entity's `Velocity`.
#[derive(Debug, Clone)]
struct Position {
    /// X coordinate.
    x: f32,
    /// Y coordinate.
    y: f32,
}
impl Component for Position {}

/// Velocity of an entity, applied to its `Position` every frame.
#[derive(Debug, Clone)]
struct Velocity {
    /// Speed along the X axis.
    vx: f32,
    /// Speed along the Y axis.
    vy: f32,
}
impl Component for Velocity {}

// Required for component storage
impl_trait_accessible!(dyn Component; Position, Velocity);

// ============================================================================
// Resources
// ============================================================================

/// Frame timing shared across all systems.
///
/// Written by `time_system` and read by `display_system` each frame.
#[derive(Debug)]
struct ProjectTime {
    /// Seconds elapsed since the previous frame.
    delta: f32,
    /// Total seconds since the loop started.
    elapsed: f32,
}
impl Resource for ProjectTime {}

/// Total points awarded so far, written by `scoring_system`.
#[derive(Debug)]
struct Score(u32);
impl Resource for Score {}

// ============================================================================
// Systems
// ============================================================================

/// Update the global project time.
///
/// - `ResMut<ProjectTime>` ⇒ writes the resource (scheduler prevents parallel
///   writes to ProjectTime).
fn time_system(mut time: ResMut<ProjectTime>) {
    let Some(mut t) = time.get_mut() else { return };
    t.elapsed += t.delta;
}

/// Apply velocity to position for every entity.
///
/// - `Query<&mut Position, &Velocity>` ⇒ writes Position, reads Velocity.
fn movement_system(mut query: Query<(&mut Position, &Velocity)>) {
    for (mut pos, vel) in query.iter_mut() {
        pos.x += vel.vx;
        pos.y += vel.vy;
    }
}

/// Award points based on an entity's distance from (0,0).
///
/// - `Query<&Position>`    ⇒ reads Position.
/// - `ResMut<Score>`       ⇒ writes Score.
fn scoring_system(mut query: Query<&Position>, mut score: ResMut<Score>) {
    let Some(mut s) = score.get_mut() else { return };

    for pos in query.iter_mut() {
        let dist = (pos.x * pos.x + pos.y * pos.y).sqrt();
        s.0 += dist as u32;
    }
}

/// Print current project stats.
///
/// - `Query<(Entity, &Position)>` ⇒ reads Position (and entity id).
/// - `Res<ProjectTime>`              ⇒ reads ProjectTime.
/// - `Res<Score>`                 ⇒ reads Score.
fn display_system(
    mut query: Query<(Entity, &Position)>,
    time: Res<ProjectTime>,
    score: Res<Score>,
) {
    let t = time.get().unwrap();
    let s = score.get().unwrap();

    println!("--- Frame stats @ {:.3}s ---", t.elapsed);
    println!("  score: {:>5}", s.0);
    for (entity, pos) in query.iter_mut() {
        println!("  entity {:?} at ({:.1}, {:.1})", entity, pos.x, pos.y);
    }
}

// ============================================================================
// Entry Point
// ============================================================================

/// Entry point: builds the demo world, registers components, resources, and
/// systems, then runs a few frames to show the scheduler's batch splitting.
fn main() {
    println!("=== Resources + Components Demo ===\n");

    let mut engine = Engine::new();
    engine.set_parallel_execution(true);

    // Step 1: Register component and resource types with the world.
    engine.world_mut().register_component::<Position>();
    engine.world_mut().register_component::<Velocity>();

    // Insert the initial resources.
    engine.world_mut().insert_resource(ProjectTime {
        delta: 0.016,
        elapsed: 0.0,
    });
    engine.world_mut().insert_resource(Score(0));

    // Step 2: Register systems and print the scheduler's execution graph.
    // The scheduler analyses each system's access pattern *before* any frame:
    //   time_system      ⇒ writes ProjectTime
    //   movement_system  ⇒ writes Position, reads Velocity
    //   scoring_system   ⇒ reads Position, writes Score
    //   display_system   ⇒ reads Position, reads ProjectTime, reads Score
    //
    // Conflicts found by the scheduler:
    //   • time_system (w ProjectTime) ⇏ display_system (r ProjectTime)  → batch split
    //   • movement_system (w Pos)  ⇏ scoring_system (r Pos)       → batch split
    //   • movement_system (w Pos)  ⇏ display_system (r Pos)       → batch split
    //   • scoring_system  (w Score) ⇏ display_system (r Score)     → batch split
    engine.register_system("time", time_system);
    engine.register_system("movement", movement_system);
    engine.register_system("scoring", scoring_system);
    engine.register_system("display", display_system);

    engine.print_execution_graph();

    // Step 3: Spawn three entities with distinct initial positions and
    // velocities.
    for i in 0..3 {
        engine
            .world_mut()
            .create_entity()
            .with(Position {
                x: i as f32 * 10.0,
                y: 0.0,
            })
            .with(Velocity {
                vx: 1.0,
                vy: (i as f32) * 0.5,
            })
            .build()
            .unwrap();
    }

    // Step 4: Run a few frames, then report the final state.
    for _frame in 0..3 {
        engine.process_frame().unwrap();
    }

    println!("\nDemo complete!");
}
