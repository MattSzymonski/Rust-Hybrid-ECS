//! `change_detection_demo` — a runnable example demonstrating Pill's
//! change-detection filters (`Changed<T>`, `Added<T>`, `With<T>`,
//! `Without<T>`) on top of the parameterized `Query<Data, Filter>` API.
//!
//! Each frame three systems run:
//! - `movement_system` mutates `Position` for entities that have a
//!   non-zero `Velocity`. The mutation goes through `Mut<Position>`,
//!   which bumps the per-row `changed` tick automatically.
//! - `react_to_movement_system` uses `Query<(Entity, &Position),
//!   Changed<Position>>` and only sees entities whose position was
//!   actually mutated since this system last ran.
//! - `react_to_new_health_system` uses `Query<(Entity,),
//!   Added<Health>>` to react when Health appears on an entity for the
//!   first time. Commands add Health to entity 0 on frame 3.
//!
//! # Responsibilities
//!
//! - Defines the `Position`, `Velocity`, `Health`, and `Player`
//!   components plus the `FrameCounter` and `TargetEntity` resources.
//! - Registers five systems that exercise the `Changed<T>`, `Added<T>`,
//!   `With<T>`, and `Without<T>` change-detection filters.
//! - Runs five deterministic frames and prints the reactive output so the
//!   demo's behaviour can be verified from the log.
//!
//! # Design
//!
//! The demo runs the engine sequentially (`set_parallel_execution(false)`)
//! so the printed log is deterministic. `driver_system` runs first: it
//! advances the frame counter and queues a `Health` attachment through
//! deferred `Commands`, so `Added<Health>` picks the new component up on
//! the following frame.

// External crates
use pill_engine::{
    Added, Changed, Commands, Component, Engine, Entity, Query, ResMut, Resource, With, Without,
};
use trait_type_map::impl_trait_accessible;

// ============================================================================
// Position
// ============================================================================

/// 2D position component used by the demo's movement simulation.
///
/// `movement_system` mutates this component through `Mut<Position>`, which
/// bumps the per-row change tick so `Changed<Position>` filters observe it.
#[derive(Debug, Clone)]
struct Position {
    /// Horizontal coordinate in world units.
    x: f32,
    /// Vertical coordinate in world units.
    y: f32,
}
impl Component for Position {}

// ============================================================================
// Velocity
// ============================================================================

/// 2D velocity component describing per-frame movement.
///
/// Only entities with a non-zero `Velocity` are moved by `movement_system`,
/// so stationary entities never trigger `Changed<Position>`.
#[derive(Debug, Clone)]
struct Velocity {
    /// Horizontal velocity applied on each moved frame.
    x: f32,
    /// Vertical velocity applied on each moved frame.
    y: f32,
}
impl Component for Velocity {}

// ============================================================================
// Health
// ============================================================================

/// Hit-points component attached to the target entity on frame 3.
///
/// Exists solely to exercise the `Added<Health>` and `Without<Health>`
/// filters; the demo never reads the value back.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct Health(i32);
impl Component for Health {}

// ============================================================================
// Player
// ============================================================================

/// Marker so we can scope iteration with `With<Player>` / `Without<Player>`.
///
/// A unit component; it carries no data, only its presence on an entity.
#[derive(Debug, Clone)]
struct Player;
impl Component for Player {}

// ============================================================================
// Trait-object registration
// ============================================================================

impl_trait_accessible!(dyn Component; Position, Velocity, Health, Player);

// ============================================================================
// FrameCounter
// ============================================================================

/// Tracks the current frame so the demo only mutates on selected frames.
#[derive(Debug)]
struct FrameCounter(u32);
impl Resource for FrameCounter {}

// ============================================================================
// TargetEntity
// ============================================================================

/// Stores the entity id we want to give Health to on frame 3.
#[derive(Debug)]
struct TargetEntity(Entity);
impl Resource for TargetEntity {}

// ============================================================================
// Systems
// ============================================================================

/// Mutates Position only on even frames so reactive systems can see the
/// difference between "changed" and "unchanged" frames.
fn movement_system(frame: ResMut<FrameCounter>, mut q: Query<(&mut Position, &Velocity)>) {
    // Step 1: read the current frame and skip odd frames entirely so the
    // reactive systems observe a genuine "unchanged" frame.
    let frame_no = frame.get().map(|f| f.0).unwrap_or(0);
    if !frame_no.is_multiple_of(2) {
        return; // odd frames: skip mutation entirely
    }

    // Step 2: integrate velocity into position for every matching entity.
    for (mut pos, vel) in q.iter_mut() {
        pos.x += vel.x;
        pos.y += vel.y;
    }
}

/// Reacts only to entities whose Position was mutated since this system
/// last ran. Combines a row-level filter (`Changed<Position>`) with an
/// archetype-level filter (`With<Player>`) to scope the result.
// The full query type is spelled out on purpose: combining a row filter with
// an archetype filter is exactly what this example exists to show, and hiding
// it behind a type alias would move the demonstration out of view.
#[allow(clippy::type_complexity)]
fn react_to_movement_system(mut q: Query<(Entity, &Position), (Changed<Position>, With<Player>)>) {
    let mut count = 0;
    for (entity, pos) in q.iter_mut() {
        println!(
            "  [react_to_movement] player entity {} moved -> ({:.1}, {:.1})",
            entity.id(),
            pos.x,
            pos.y
        );
        count += 1;
    }
    if count == 0 {
        println!("  [react_to_movement] no player movement this frame");
    }
}

/// Reacts to entities that have a Position but explicitly NO Health, to
/// demonstrate the `Without<T>` filter.
fn report_unhealthy_system(mut q: Query<(Entity,), (With<Position>, Without<Health>)>) {
    let ids: Vec<u64> = q.iter_mut().map(|(e,)| e.id()).collect();
    println!("  [report_unhealthy] entities w/o Health: {:?}", ids);
}

/// Detects newly attached Health components.
fn react_to_new_health_system(mut q: Query<(Entity,), Added<Health>>) {
    for (entity,) in q.iter_mut() {
        println!(
            "  [react_to_new_health] entity {} just gained Health!",
            entity.id()
        );
    }
}

/// Drives the simulation: bumps the frame counter and on frame 3 attaches
/// a Health component to the target entity (via deferred Commands so the
/// Added<Health> filter picks it up the following frame).
fn driver_system(
    mut commands: Commands,
    mut frame: ResMut<FrameCounter>,
    target: ResMut<TargetEntity>,
) {
    // Step 1: advance the frame counter for this frame.
    let mut f = frame.get_mut().unwrap();
    f.0 += 1;
    let frame_no = f.0;

    let target_entity = target.get().unwrap().0;

    // Step 2: on frame 3, queue the Health attachment through deferred
    // Commands so `Added<Health>` observes it on the next frame.
    if frame_no == 3 {
        println!(
            "  [driver] queueing Health for entity {}",
            target_entity.id()
        );
        commands.add_component_to_entity(target_entity, Health(100));
    }
}

// ============================================================================
// Main
// ============================================================================

/// Entry point that builds the engine, registers components and systems,
/// and runs five frames of the change-detection demo.
fn main() {
    println!("=== Change Detection Demo ===\n");

    // Step 1: create the engine.
    let mut engine = Engine::new();
    // Run sequentially so the printed log is deterministic.
    engine.set_parallel_execution(false);

    // Step 2: register every component the demo's queries reference.
    let world = engine.world_mut();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();
    world.register_component::<Player>();

    // Step 3: spawn the demo's three entities.
    // Two players (one moving, one stationary) and one non-player.
    let player_a = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .with(Velocity { x: 1.0, y: 0.5 })
        .with(Player)
        .build()
        .unwrap();

    let _player_b = world
        .create_entity()
        .with(Position { x: 10.0, y: 10.0 })
        .with(Velocity { x: 0.0, y: 0.0 }) // never moves -> never "changes"
        .with(Player)
        .build()
        .unwrap();

    let _npc = world
        .create_entity()
        .with(Position { x: -5.0, y: 0.0 })
        .with(Velocity { x: 0.0, y: 0.0 })
        .build()
        .unwrap();

    // Step 4: seed the resources the systems read each frame.
    world.insert_resource(FrameCounter(0));
    world.insert_resource(TargetEntity(player_a));

    // Step 5: register the systems in execution order.
    // Driver runs first to advance the frame counter and queue Commands.
    engine.register_system("driver", driver_system);
    engine.register_system("movement", movement_system);
    engine.register_system("react_to_movement", react_to_movement_system);
    engine.register_system("react_to_new_health", react_to_new_health_system);
    engine.register_system("report_unhealthy", report_unhealthy_system);

    // Step 6: run five frames and print each one's reactive output.
    for frame in 1..=5 {
        println!("--- Frame {} ---", frame);
        engine.process_frame().unwrap();
        println!();
    }
}
