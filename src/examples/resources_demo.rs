// ----------------------------------------------------------------------------
// Demo: Resources + Components + Systems Together
// ----------------------------------------------------------------------------
//! Demonstrates using resources (Res/ResMut) alongside component queries
//! in a single frame loop. The scheduler respects resource access patterns
//! when building the dependency graph for parallel execution.
//!
//! Scenario: A simple game loop with:
//! - GameTime resource (delta time, elapsed)
//! - Score resource   (total points)
//! - Position / Velocity components on entities
//!
//! Systems:
//! - `time_system`     – updates GameTime (ResMut<GameTime>)
//! - `movement_system` – moves entities (Query<&mut Position, &Velocity>)
//! - `scoring_system`  – awards points based on distance from origin
//!                       (Query<&Position>, ResMut<Score>)
//! - `display_system`  – prints stats (Query<&Position>, Res<GameTime>, Res<Score>)

use ecs_hybrid::*;
use trait_type_map::impl_trait_accessible;

// ----------------------------------------------------------------------------
// Components
// ----------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Position {
    x: f32,
    y: f32,
}
impl Component for Position {}

#[derive(Debug, Clone)]
struct Velocity {
    vx: f32,
    vy: f32,
}
impl Component for Velocity {}

// Required for component storage
impl_trait_accessible!(dyn Component; Position, Velocity);

// ----------------------------------------------------------------------------
// Resources
// ----------------------------------------------------------------------------

#[derive(Debug)]
struct GameTime {
    delta: f32,
    elapsed: f32,
}
impl Resource for GameTime {}

#[derive(Debug)]
struct Score(u32);
impl Resource for Score {}

// ----------------------------------------------------------------------------
// Systems
// ----------------------------------------------------------------------------

/// Update the global game time.
///
/// - `ResMut<GameTime>` ⇒ writes the resource (scheduler prevents parallel
///   writes to GameTime).
fn time_system(mut time: ResMut<GameTime>) {
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

/// Print current game stats.
///
/// - `Query<(Entity, &Position)>` ⇒ reads Position (and entity id).
/// - `Res<GameTime>`              ⇒ reads GameTime.
/// - `Res<Score>`                 ⇒ reads Score.
fn display_system(mut query: Query<(Entity, &Position)>, time: Res<GameTime>, score: Res<Score>) {
    let t = time.get().unwrap();
    let s = score.get().unwrap();

    println!("--- Frame stats @ {:.3}s ---", t.elapsed);
    println!("  score: {:>5}", s.0);
    for (entity, pos) in query.iter_mut() {
        println!("  entity {:?} at ({:.1}, {:.1})", entity, pos.x, pos.y);
    }
}

// ----------------------------------------------------------------------------
// Main
// ----------------------------------------------------------------------------

pub(crate) fn main() {
    println!("=== Resources + Components Demo ===\n");

    let mut engine = Engine::new();
    engine.set_parallel_execution(true);

    // --- Setup: register component & resource types ------------------------
    engine.world_mut().register_component::<Position>();
    engine.world_mut().register_component::<Velocity>();

    // Insert initial resources
    engine.world_mut().insert_resource(GameTime {
        delta: 0.016,
        elapsed: 0.0,
    });
    engine.world_mut().insert_resource(Score(0));

    // --- Register systems --------------------------------------------------
    // The scheduler analyses each system's access pattern *before* any frame:
    //   time_system      ⇒ writes GameTime
    //   movement_system  ⇒ writes Position, reads Velocity
    //   scoring_system   ⇒ reads Position, writes Score
    //   display_system   ⇒ reads Position, reads GameTime, reads Score
    //
    // Conflicts found by the scheduler:
    //   • time_system (w GameTime) ⇏ display_system (r GameTime)  → batch split
    //   • movement_system (w Pos)  ⇏ scoring_system (r Pos)       → batch split
    //   • movement_system (w Pos)  ⇏ display_system (r Pos)       → batch split
    //   • scoring_system  (w Score) ⇏ display_system (r Score)     → batch split
    engine.register_system("time", time_system);
    engine.register_system("movement", movement_system);
    engine.register_system("scoring", scoring_system);
    engine.register_system("display", display_system);

    engine.print_execution_graph();

    // --- Spawn entities ----------------------------------------------------
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

    // --- Run a few frames --------------------------------------------------
    for _frame in 0..3 {
        engine.process_frame().unwrap();
    }

    println!("\nDemo complete!");
}
