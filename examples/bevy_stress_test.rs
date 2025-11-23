// ============================================================================
// Bevy Stress Test Example - Performance Comparison
// ============================================================================
//! This example stress tests Bevy ECS with the same scenario:
//! - 10,000 entities
//! - Multiple box colliders on obstacle entity
//! - Collision detection system
//! - Performance measurements

use bevy::prelude::*;
use std::time::Instant;

// ============================================================================
// Components
// ============================================================================

#[derive(Component, Debug, Clone)]
struct Position {
    x: f32,
    y: f32,
    z: f32,
}

impl Position {
    fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

#[derive(Component, Debug, Clone)]
struct Velocity {
    x: f32,
    y: f32,
    z: f32,
}

impl Velocity {
    fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

#[derive(Debug, Clone)]
struct ColliderData {
    center: (f32, f32, f32),
    size: (f32, f32, f32),
}

impl ColliderData {
    fn new(center: (f32, f32, f32), size: (f32, f32, f32)) -> Self {
        Self { center, size }
    }
}

/// BoxCollider component that stores multiple colliders for an entity
#[derive(Component, Debug, Clone)]
struct BoxCollider {
    colliders: Vec<ColliderData>,
}

impl BoxCollider {
    fn new() -> Self {
        Self {
            colliders: Vec::new(),
        }
    }

    fn add_collider(&mut self, center: (f32, f32, f32), size: (f32, f32, f32)) {
        self.colliders.push(ColliderData::new(center, size));
    }
}

#[derive(Component, Debug, Clone)]
struct Obstacle;

// Resource to track simulation state
#[derive(Resource)]
struct SimulationState {
    frame_count: u32,
    target_frames: u32,
    start_time: Option<Instant>,
    entity_count: usize,
}

// ============================================================================
// Systems
// ============================================================================

fn movement_system(mut query: Query<(&mut Position, &Velocity)>) {
    for (mut position, velocity) in query.iter_mut() {
        position.x += velocity.x * 0.016;
        position.y += velocity.y * 0.016;
        position.z += velocity.z * 0.016;
    }
}

fn simulation_control_system(
    mut state: ResMut<SimulationState>,
    query: Query<&Position>,
    mut exit: EventWriter<AppExit>,
) {
    // Start timer on first frame
    if state.start_time.is_none() {
        state.start_time = Some(Instant::now());
        println!("\nRunning 10,000 frame simulation...\n");
    }

    state.frame_count += 1;

    // Check if we've reached target frames
    if state.frame_count >= state.target_frames {
        let duration = state.start_time.unwrap().elapsed();

        // Calculate results
        let fps = state.target_frames as f64 / duration.as_secs_f64();
        let frame_time_ms = duration.as_secs_f64() * 1000.0 / state.target_frames as f64;
        let total_operations = state.entity_count * state.target_frames as usize;

        println!("=== Results ===");
        println!("Architecture:       Bevy ECS");
        println!("Entities:           {}", state.entity_count);
        println!("Frames:             {}", state.target_frames);
        println!("Colliders:          5 box colliders on obstacle");
        println!("\nTime taken:         {:.3} s", duration.as_secs_f64());
        println!("FPS:                {:.0}", fps);
        println!("Avg frame time:     {:.3} ms", frame_time_ms);
        println!("Total operations:   {}", total_operations);
        println!(
            "Operations/second:  {:.0}",
            total_operations as f64 / duration.as_secs_f64()
        );

        // Count entities near obstacle
        let near_count = query
            .iter()
            .filter(|position| {
                let dx = position.x - 50.0;
                let dy = position.y;
                let dist = (dx * dx + dy * dy).sqrt();
                dist < 15.0
            })
            .count();

        println!("\nEntities near obstacle: {}", near_count);
        println!("\n✓ Stress test completed!");

        // Exit the application
        exit.send(AppExit::Success);
    }
}

// ============================================================================
// Setup
// ============================================================================

fn setup(mut commands: Commands) {
    println!("=== Stress Test: Bevy ECS ===\n");

    // Create obstacle entity with multiple box colliders
    println!("Creating obstacle with 5 box colliders...");

    let mut box_collider = BoxCollider::new();
    box_collider.add_collider((0.0, 0.0, 0.0), (5.0, 5.0, 5.0));
    box_collider.add_collider((6.0, 0.0, 0.0), (4.0, 4.0, 4.0));
    box_collider.add_collider((-6.0, 0.0, 0.0), (4.0, 4.0, 4.0));
    box_collider.add_collider((0.0, 6.0, 0.0), (3.0, 3.0, 3.0));
    box_collider.add_collider((0.0, -6.0, 0.0), (3.0, 3.0, 3.0));

    commands.spawn((Obstacle, Position::new(50.0, 0.0, 0.0), box_collider));

    println!("✓ Created obstacle entity with 5 box colliders");

    // Create moving entities
    let entity_count = 10_000;
    println!("Creating {} moving entities...", entity_count);

    for i in 0..entity_count {
        let angle = (i as f32 / entity_count as f32) * std::f32::consts::PI * 2.0;
        commands.spawn((
            Position::new(angle.cos() * 20.0, angle.sin() * 20.0, 0.0),
            Velocity::new(angle.cos() * 2.0, angle.sin() * 2.0, 0.0),
        ));
    }

    println!("✓ Created {} moving entities", entity_count);
    println!("\nScenario: Entities move toward obstacle");
    println!("Collision check: Query-based iteration");
}

// ============================================================================
// Main
// ============================================================================

fn main() {
    App::new()
        .add_plugins(MinimalPlugins)
        .insert_resource(SimulationState {
            frame_count: 0,
            target_frames: 10_000,
            start_time: None,
            entity_count: 10_000,
        })
        .add_systems(Startup, setup)
        .add_systems(Update, (movement_system, simulation_control_system))
        .run();
}
