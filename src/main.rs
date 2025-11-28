// ============================================================================
// Stress Test Example - Performance Testing (Bevy Version)
// ============================================================================
//! This example stress tests Bevy ECS with:
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

    fn intersects(&self, transform: &Position, other_pos: (f32, f32, f32)) -> bool {
        let (cx, cy, cz) = self.center;
        let (sx, sy, sz) = self.size;

        let min_x = transform.x + cx - sx / 2.0;
        let max_x = transform.x + cx + sx / 2.0;
        let min_y = transform.y + cy - sy / 2.0;
        let max_y = transform.y + cy + sy / 2.0;
        let min_z = transform.z + cz - sz / 2.0;
        let max_z = transform.z + cz + sz / 2.0;

        other_pos.0 >= min_x
            && other_pos.0 <= max_x
            && other_pos.1 >= min_y
            && other_pos.1 <= max_y
            && other_pos.2 >= min_z
            && other_pos.2 <= max_z
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

#[derive(Resource)]
struct SimulationState {
    frame_count: u32,
    max_frames: u32,
    start_time: Instant,
    entity_count: usize,
}

// ============================================================================
// Systems
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
    println!("\nScenario: Entities move toward obstacle with 5 box colliders");
    println!("Collision check: Query-based iteration");
    println!("\nRunning 10,000 frame simulation...\n");

    // Initialize simulation state
    commands.insert_resource(SimulationState {
        frame_count: 0,
        max_frames: 10_000,
        start_time: Instant::now(),
        entity_count,
    });
}

fn collision_and_movement_system(
    mut query: Query<(&mut Position, &Velocity), Without<Obstacle>>,
    obstacle_query: Query<(&Position, &BoxCollider), With<Obstacle>>,
) {
    // Get obstacle data
    let Ok((obstacle_pos, obstacle_collider)) = obstacle_query.get_single() else {
        return;
    };

    // Collect collision info: (index, new_position, should_collide)
    let mut collision_checks: Vec<(usize, f32, f32, f32, bool)> = Vec::new();

    // First pass: calculate new positions and check collisions (read-only)
    for (idx, (pos, vel)) in query.iter().enumerate() {
        let new_x = pos.x + vel.x * 0.016;
        let new_y = pos.y + vel.y * 0.016;
        let new_z = pos.z + vel.z * 0.016;

        // Check collision with all colliders on the obstacle
        let mut collided = false;
        for collider in &obstacle_collider.colliders {
            if collider.intersects(obstacle_pos, (new_x, new_y, new_z)) {
                collided = true;
                break;
            }
        }

        collision_checks.push((idx, new_x, new_y, new_z, collided));
    }

    // Second pass: apply movement only if no collision (write)
    for (idx, (mut pos, _vel)) in query.iter_mut().enumerate() {
        if let Some(&(_, new_x, new_y, new_z, collided)) = collision_checks.get(idx) {
            if !collided {
                pos.x = new_x;
                pos.y = new_y;
                pos.z = new_z;
            }
        }
    }
}

fn simulation_tracker(
    mut sim_state: ResMut<SimulationState>,
    query: Query<&Position, Without<Obstacle>>,
    mut app_exit: EventWriter<AppExit>,
) {
    sim_state.frame_count += 1;

    if sim_state.frame_count >= sim_state.max_frames {
        let duration = sim_state.start_time.elapsed();

        // Calculate results
        let frame_time_ms = duration.as_secs_f64() * 1000.0 / sim_state.max_frames as f64;
        let total_checks = sim_state.entity_count * sim_state.max_frames as usize * 5; // 5 colliders

        println!("\n=== Results ===");
        println!(
            "Entities: {} Frames: {}, Colliders: {}",
            sim_state.entity_count, sim_state.max_frames, 5
        );
        println!(
            "Total collision checks: {}, Checks per second: {:.0}",
            total_checks,
            total_checks as f64 / duration.as_secs_f64()
        );

        // Count entities near obstacle
        let near_count = query
            .iter()
            .filter(|pos| {
                let dx = pos.x - 50.0;
                let dy = pos.y;
                let dist = (dx * dx + dy * dy).sqrt();
                dist < 15.0
            })
            .count();

        println!("Entities near obstacle: {}", near_count);
        println!("Avg frame time: {:.3} ms", frame_time_ms);

        // Exit the app
        app_exit.send(AppExit::Success);
    }
}

// ============================================================================
// Main
// ============================================================================

fn main() {
    App::new()
        .add_plugins(MinimalPlugins)
        .add_systems(Startup, setup)
        .add_systems(Update, (collision_and_movement_system, simulation_tracker))
        .run();
}
