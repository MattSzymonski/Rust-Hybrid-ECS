// ============================================================================
// Stress Test Example - Performance Testing
// ============================================================================
//! This example stress tests the ECS with:
//! - 10,000 entities
//! - Multiple box colliders on obstacle entity
//! - Collision detection system
//! - Performance measurements

use ecs_hybrid::{Component, Engine, GlobalComponentQuery, Query};
use std::time::Instant;
use trait_type_map::impl_trait_accessible;

// ============================================================================
// Components
// ============================================================================

#[derive(Debug, Clone)]
struct SimulationStats {
    frame_count: u32,
    max_frames: u32,
    start_time: Instant,
    entity_count: usize,
}

impl Component for SimulationStats {}

#[derive(Debug, Clone)]
struct Transform {
    x: f32,
    y: f32,
    z: f32,
}

impl Transform {
    fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

impl Component for Transform {}

#[derive(Debug, Clone)]
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

impl Component for Velocity {}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct ColliderData {
    center: (f32, f32, f32),
    size: (f32, f32, f32),
}

impl ColliderData {
    fn new(center: (f32, f32, f32), size: (f32, f32, f32)) -> Self {
        Self { center, size }
    }

    fn intersects(&self, transform: &Transform, other_pos: (f32, f32, f32)) -> bool {
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
#[derive(Debug, Clone)]
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

    fn check_any_collision(&self, transform: &Transform, other_pos: (f32, f32, f32)) -> bool {
        self.colliders
            .iter()
            .any(|collider| collider.intersects(transform, other_pos))
    }
}

impl Component for BoxCollider {}

#[derive(Debug, Clone)]
struct Obstacle;

impl Component for Obstacle {}

// Make all components accessible via the Component trait for TraitTypeMap
impl_trait_accessible!(dyn Component; SimulationStats, Transform, Velocity, BoxCollider, Obstacle);

// ============================================================================
// Systems
// ============================================================================

fn collision_and_movement_system(
    mut moving_query: Query<(&mut Transform, &Velocity)>,
    mut obstacle_query: Query<(&Transform, &BoxCollider, &Obstacle)>,
) {
    // Get obstacle data
    let mut obstacle_data = None;
    for (obs_transform, obs_collider, _) in obstacle_query.iter_mut() {
        obstacle_data = Some((obs_transform.clone(), obs_collider.clone()));
        break;
    }

    let Some((obstacle_transform, obstacle_collider)) = obstacle_data else {
        return;
    };

    // Collect collision info: (new_position, should_collide)
    let mut collision_checks: Vec<(f32, f32, f32, bool)> = Vec::new();

    // First pass: calculate new positions and check collisions (read-only)
    for (transform, velocity) in moving_query.iter_mut() {
        let new_x = transform.x + velocity.x * 0.016;
        let new_y = transform.y + velocity.y * 0.016;
        let new_z = transform.z + velocity.z * 0.016;

        // Check collision with all colliders on the obstacle
        let collided =
            obstacle_collider.check_any_collision(&obstacle_transform, (new_x, new_y, new_z));

        collision_checks.push((new_x, new_y, new_z, collided));
    }

    // Second pass: apply movement only if no collision (write)
    for ((transform, _velocity), (new_x, new_y, new_z, collided)) in
        moving_query.iter_mut().zip(collision_checks.iter())
    {
        if !collided {
            transform.x = *new_x;
            transform.y = *new_y;
            transform.z = *new_z;
        }
    }
}

fn simulation_tracker_system(mut stats: GlobalComponentQuery<SimulationStats>) {
    if let Some(stats) = stats.get_mut() {
        stats.frame_count += 1;

        if stats.frame_count >= stats.max_frames {
            let duration = stats.start_time.elapsed();

            // Calculate results
            let total_checks = stats.entity_count * stats.max_frames as usize * 5; // 5 colliders

            println!(
                "Entities: {} Frames: {}, Colliders: {}",
                stats.entity_count, stats.max_frames, 5
            );
            println!(
                "Total collision checks: {}, Checks per second: {:.0}",
                total_checks,
                total_checks as f64 / duration.as_secs_f64()
            );
        }
    }
}

// ============================================================================
// Main
// ============================================================================

pub fn main() {
    println!("=== Stress Test: Archetype-Based ECS ===\n");

    let mut engine = Engine::new();

    // Register all component types before use
    engine.world_mut().register_component::<SimulationStats>();
    engine.world_mut().register_component::<Transform>();
    engine.world_mut().register_component::<Velocity>();
    engine.world_mut().register_component::<BoxCollider>();
    engine.world_mut().register_component::<Obstacle>();

    // Register systems
    engine.register_system("collision_and_movement", collision_and_movement_system);
    engine.register_system("simulation_tracker", simulation_tracker_system);

    // Create obstacle entity with multiple box colliders
    println!("Creating obstacle with 5 box colliders...");

    // Create a BoxCollider with 5 different colliders
    let mut box_collider = BoxCollider::new();
    box_collider.add_collider((0.0, 0.0, 0.0), (5.0, 5.0, 5.0));
    box_collider.add_collider((6.0, 0.0, 0.0), (4.0, 4.0, 4.0));
    box_collider.add_collider((-6.0, 0.0, 0.0), (4.0, 4.0, 4.0));
    box_collider.add_collider((0.0, 6.0, 0.0), (3.0, 3.0, 3.0));
    box_collider.add_collider((0.0, -6.0, 0.0), (3.0, 3.0, 3.0));

    let _obstacle = engine
        .world_mut()
        .create_entity()
        .with(Obstacle)
        .with(Transform::new(50.0, 0.0, 0.0))
        .with(box_collider)
        .build();

    println!("✓ Created obstacle entity with 5 box colliders");

    // Create moving entities
    let entity_count = 10_000;
    println!("Creating {} moving entities...", entity_count);

    for i in 0..entity_count {
        let angle = (i as f32 / entity_count as f32) * std::f32::consts::PI * 2.0;
        engine
            .world_mut()
            .create_entity()
            .with(Transform::new(angle.cos() * 20.0, angle.sin() * 20.0, 0.0))
            .with(Velocity::new(angle.cos() * 2.0, angle.sin() * 2.0, 0.0))
            .build();
    }

    println!("✓ Created {} moving entities", entity_count);
    println!("\nScenario: Entities move toward obstacle with 5 box colliders");
    println!("Collision check: Query-based iteration");
    println!("\nRunning 10,000 frame simulation...\n");

    // Set up simulation stats as global component
    let max_frames = 10_000;
    engine.world_mut().add_global_component(SimulationStats {
        frame_count: 0,
        max_frames,
        start_time: Instant::now(),
        entity_count,
    });

    // Run simulation with systems
    for _frame in 0..max_frames {
        engine.process_frame();
    }

    // Count entities near obstacle
    let mut count_query = Query::<(&Transform,)>::new(engine.world_mut());
    let near_count = count_query
        .iter_mut()
        .filter(|(transform,)| {
            let dx = transform.x - 50.0;
            let dy = transform.y;
            let dist = (dx * dx + dy * dy).sqrt();
            dist < 15.0
        })
        .count();

    println!("Entities near obstacle: {}", near_count);

    // Calculate average frame time
    let stats_query = GlobalComponentQuery::<SimulationStats>::new(engine.world_mut());
    let frame_time_ms = if let Some(stats) = stats_query.get() {
        let duration = stats.start_time.elapsed();
        duration.as_millis() as f64 / stats.max_frames as f64
    } else {
        0.0
    };

    println!("Avg frame time: {:.3} ms", frame_time_ms);
}
