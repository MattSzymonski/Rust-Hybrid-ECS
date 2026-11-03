//! A runnable example of Pill's change-detection filters (`Changed<T>`,
//! `Added<T>`, `With<T>`, `Without<T>`) on top of the parameterized
//! `Query<Data, Filter>` API.
//!
//! Every frame runs five systems. On even frames `movement_system` writes
//! `Position` through `Mut<Position>`, which sets the row's `changed` tick
//! whether or not the value moved. `react_to_movement_system` reports those
//! rows through `Changed<Position>`. `react_to_new_health_system` reports a
//! `Health` that the driver queued through deferred `Commands`, one frame
//! later.
//!
//! # Responsibilities
//!
//! - Defines the `Position`, `Velocity`, `Health`, and `Player` components plus
//!   the `FrameCounter` and `TargetEntity` resources.
//! - Registers five systems that exercise the four change-detection filters.
//! - Runs five deterministic frames and prints the reactive output so the
//!   demo's behaviour can be checked from the log.
//!
//! # Design
//!
//! The engine runs sequentially (`set_parallel_execution(false)`) so the
//! printed log has a fixed order. The driver runs first: it advances the frame
//! counter and queues the `Health` attachment, so `Added<Health>` sees it on
//! the following frame.

// External crates
use pill_engine::{
    Added, Changed, Commands, Component, Engine, Entity, Query, ResMut, Resource, With, Without,
};

// ============================================================================
// Position
// ============================================================================

/// 2D position of an entity in the demo's movement simulation.
///
/// `movement_system` writes it through `Mut<Position>`, which marks the row as
/// changed so the `Changed<Position>` filter can see it.
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

/// 2D velocity, added to `Position` on every frame that moves.
///
/// Every entity that carries it matches the `movement_system` query, which adds
/// the velocity to `Position` on even frames.
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

/// Hit points, attached to the target entity on frame 3.
///
/// It exists only to exercise the `Added<Health>` and `Without<Health>`
/// filters, and the demo never reads the value back.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct Health(i32);
impl Component for Health {}

// ============================================================================
// Player
// ============================================================================

/// Marks an entity as a player for the `With<Player>` filter.
///
/// A unit component: it carries no data, and only its presence matters.
#[derive(Debug, Clone)]
struct Player;
impl Component for Player {}

// ============================================================================
// FrameCounter
// ============================================================================

/// Counts frames, so the demo changes data only on the frames it chooses.
#[derive(Debug)]
struct FrameCounter(u32);
impl Resource for FrameCounter {}

// ============================================================================
// TargetEntity
// ============================================================================

/// Stores the entity that gains a `Health` component on frame 3.
#[derive(Debug)]
struct TargetEntity(Entity);
impl Resource for TargetEntity {}

// ============================================================================
// Systems
// ============================================================================

/// Writes `Position` only on even frames, so the reactive systems see both
/// changed and unchanged frames.
///
/// The loop writes through `Mut` on every matching entity, so a zero
/// `Velocity` still marks the row as changed. The filter reports a write, not
/// a move.
fn movement_system(
    frame_counter: ResMut<FrameCounter>,
    mut query: Query<(&mut Position, &Velocity)>,
) {
    // Step 1: read the frame number and skip odd frames, so the reactive
    // systems also see a frame in which nothing changed.
    let frame_number = frame_counter.get().map(|counter| counter.0).unwrap_or(0);
    if !frame_number.is_multiple_of(2) {
        return;
    }

    // Step 2: add the velocity to the position of every matching entity.
    for (mut position, velocity) in query.iter_mut() {
        position.x += velocity.x;
        position.y += velocity.y;
    }
}

/// Logs the player entities whose `Position` changed since this system last
/// ran.
///
/// It combines a row filter (`Changed<Position>`) with an archetype filter
/// (`With<Player>`) to narrow the result.
// The long query type stays spelled out: this example exists to show a row
// filter and an archetype filter side by side, and a type alias would hide the
// part being demonstrated.
#[allow(clippy::type_complexity)]
fn react_to_movement_system(
    mut query: Query<(Entity, &Position), (Changed<Position>, With<Player>)>,
) {
    let mut changed_entity_count = 0;
    for (entity, position) in query.iter_mut() {
        pill_core::info!(
            "  [react_to_movement] player entity {} moved -> ({:.1}, {:.1})",
            entity.id(),
            position.x,
            position.y
        );
        changed_entity_count += 1;
    }
    if changed_entity_count == 0 {
        pill_core::info!("  [react_to_movement] no player movement this frame");
    }
}

/// Logs every entity that has a `Position` and no `Health`, which is what the
/// `Without<T>` filter selects.
fn report_unhealthy_system(mut query: Query<(Entity,), (With<Position>, Without<Health>)>) {
    let entity_ids: Vec<u64> = query.iter_mut().map(|(entity,)| entity.id()).collect();
    pill_core::info!("  [report_unhealthy] entities w/o Health: {:?}", entity_ids);
}

/// Logs every entity whose `Health` was added since this system last ran.
fn react_to_new_health_system(mut query: Query<(Entity,), Added<Health>>) {
    for (entity,) in query.iter_mut() {
        pill_core::info!(
            "  [react_to_new_health] entity {} just gained Health!",
            entity.id()
        );
    }
}

/// Drives the simulation: it adds one to the frame counter, and on frame 3 it
/// attaches a `Health` component to the target entity.
///
/// The attachment goes through deferred `Commands`, so the `Added<Health>`
/// filter sees it on the following frame.
fn simulation_driver_system(
    mut commands: Commands,
    mut frame_counter: ResMut<FrameCounter>,
    target_entity: ResMut<TargetEntity>,
) {
    // Step 1: advance the frame counter and read the new frame number.
    let mut counter = frame_counter.get_mut().unwrap();
    counter.0 += 1;
    let frame_number = counter.0;

    let health_recipient = target_entity.get().unwrap().0;

    // Step 2: on frame 3, queue the `Health` attachment through deferred
    // `Commands` so `Added<Health>` sees it on the next frame.
    if frame_number == 3 {
        pill_core::info!(
            "  [driver] queueing Health for entity {}",
            health_recipient.id()
        );
        commands.add_component_to_entity(health_recipient, Health(100));
    }
}

// ============================================================================
// Main
// ============================================================================

/// Builds the engine, registers the components and systems, and runs five
/// frames of the change-detection demo.
///
/// # Panics
///
/// Panics if an entity fails to build, which would mean the demo's setup is
/// wrong.
fn main() {
    // Install the engine's logger, so this program's lines and the engine's
    // own reports are printed.
    let _ = pill_core::telemetry::TelemetryBuilder::new().init();
    pill_core::info!("=== Change Detection Demo ===");

    // Step 1: create the engine.
    let mut engine = Engine::new();
    // Run the systems one at a time, so the log lines always come out in the
    // same order.
    engine.set_parallel_execution(false);

    // Step 2: register every component the demo's queries reference.
    let world = engine.world_mut();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();
    world.register_component::<Player>();

    // Step 3: spawn the demo's three entities.
    // Two players (one moving, one stationary) and one non-player.
    let moving_player = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .with(Velocity { x: 1.0, y: 0.5 })
        .with(Player)
        .build()
        .unwrap();

    let _stationary_player = world
        .create_entity()
        .with(Position { x: 10.0, y: 10.0 })
        .with(Velocity { x: 0.0, y: 0.0 }) // zero velocity: the row still changes
        .with(Player)
        .build()
        .unwrap();

    let _non_player = world
        .create_entity()
        .with(Position { x: -5.0, y: 0.0 })
        .with(Velocity { x: 0.0, y: 0.0 })
        .build()
        .unwrap();

    // Step 4: seed the resources the systems read each frame.
    world.insert_resource(FrameCounter(0));
    world.insert_resource(TargetEntity(moving_player));

    // Step 5: register the systems in execution order.
    // The driver runs first: it advances the frame counter and queues the
    // `Health` command.
    engine.register_system("driver", simulation_driver_system);
    engine.register_system("movement", movement_system);
    engine.register_system("react_to_movement", react_to_movement_system);
    engine.register_system("react_to_new_health", react_to_new_health_system);
    engine.register_system("report_unhealthy", report_unhealthy_system);

    // Step 6: run five frames and print each one's reactive output.
    for frame_number in 1..=5 {
        pill_core::info!("--- Frame {} ---", frame_number);
        engine.process_frame().unwrap();
    }
}
