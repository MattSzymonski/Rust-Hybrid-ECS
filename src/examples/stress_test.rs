// ============================================================================
// Stress Test Example - Performance Testing
// ============================================================================
//! This example stress tests the ECS with:
//! - 10,000 entities
//! - Multiple box colliders on obstacle entity
//! - Collision detection system
//! - Performance measurements

use crate::{Component, Query, World};
use std::time::Instant;
use trait_type_map::impl_trait_accessible;

// ============================================================================
// Components
// ============================================================================

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

    #[allow(dead_code)]
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

    #[allow(dead_code)]
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
impl_trait_accessible!(dyn Component; Transform, Velocity, BoxCollider, Obstacle);

// ============================================================================
// Systems
// ============================================================================

fn collision_and_movement_system(
    world: &mut World,
    obstacle_transform: Transform,
    obstacle_collider: &BoxCollider,
) {
    // Collect collision info: (index, new_position, should_collide)
    let mut collision_checks: Vec<(usize, f32, f32, f32, bool)> = Vec::new();

    // First pass: calculate new positions and check collisions (read-only)
    let mut query = Query::<(&Transform, &Velocity)>::new(world);
    for (idx, (transform, velocity)) in query.iter_mut().enumerate() {
        let new_x = transform.x + velocity.x * 0.016;
        let new_y = transform.y + velocity.y * 0.016;
        let new_z = transform.z + velocity.z * 0.016;

        // Check collision with all colliders on the obstacle
        let mut collided = false;
        for collider in &obstacle_collider.colliders {
            if collider.intersects(&obstacle_transform, (new_x, new_y, new_z)) {
                collided = true;
                break;
            }
        }

        collision_checks.push((idx, new_x, new_y, new_z, collided));
    }

    // Second pass: apply movement only if no collision (write)
    let mut update_query = Query::<(&mut Transform, &Velocity)>::new(world);
    for (idx, (transform, _velocity)) in update_query.iter_mut().enumerate() {
        if let Some(&(_, new_x, new_y, new_z, collided)) = collision_checks.get(idx) {
            if !collided {
                transform.x = new_x;
                transform.y = new_y;
                transform.z = new_z;
            }
        }
    }
}

// ============================================================================
// Main
// ============================================================================

pub fn main() {
    println!("=== Stress Test: Archetype-Based ECS ===\n");

    let mut world = World::new();

    // Register all component types before use
    world.register_component::<Transform>();
    world.register_component::<Velocity>();
    world.register_component::<BoxCollider>();
    world.register_component::<Obstacle>();

    // Create obstacle entity with multiple box colliders
    println!("Creating obstacle with 5 box colliders...");

    // Create a BoxCollider with 5 different colliders
    let mut box_collider = BoxCollider::new();
    box_collider.add_collider((0.0, 0.0, 0.0), (5.0, 5.0, 5.0));
    box_collider.add_collider((6.0, 0.0, 0.0), (4.0, 4.0, 4.0));
    box_collider.add_collider((-6.0, 0.0, 0.0), (4.0, 4.0, 4.0));
    box_collider.add_collider((0.0, 6.0, 0.0), (3.0, 3.0, 3.0));
    box_collider.add_collider((0.0, -6.0, 0.0), (3.0, 3.0, 3.0));

    let obstacle_transform = Transform::new(50.0, 0.0, 0.0);
    let obstacle_collider = box_collider.clone();

    let _obstacle = world
        .spawn()
        .with(Obstacle)
        .with(obstacle_transform.clone())
        .with(box_collider)
        .build();

    println!("✓ Created obstacle entity with 5 box colliders");

    // Create moving entities
    let entity_count = 10_000;
    println!("Creating {} moving entities...", entity_count);

    for i in 0..entity_count {
        let angle = (i as f32 / entity_count as f32) * std::f32::consts::PI * 2.0;
        world
            .spawn()
            .with(Transform::new(angle.cos() * 20.0, angle.sin() * 20.0, 0.0))
            .with(Velocity::new(angle.cos() * 2.0, angle.sin() * 2.0, 0.0))
            .build();
    }

    println!("✓ Created {} moving entities", entity_count);
    println!("\nScenario: Entities move toward obstacle with 5 box colliders");
    println!("Collision check: Query-based iteration");
    println!("\nRunning 10,000 frame simulation...\n");

    let frame_count = 10_000;
    let start = Instant::now();

    // Run simulation with collision detection
    for _frame in 0..frame_count {
        collision_and_movement_system(&mut world, obstacle_transform.clone(), &obstacle_collider);
    }

    let duration = start.elapsed();

    // Calculate results
    let fps = frame_count as f64 / duration.as_secs_f64();
    let frame_time_ms = duration.as_secs_f64() * 1000.0 / frame_count as f64;
    let total_checks = entity_count * frame_count * 5; // 5 colliders

    println!("=== Results ===");
    println!("Architecture:       Archetype-based");
    println!("Entities:           {}", entity_count);
    println!("Frames:             {}", frame_count);
    println!("Colliders:          5 box colliders on obstacle");
    println!("\nTime taken:         {:.3} s", duration.as_secs_f64());
    println!("FPS:                {:.0}", fps);
    println!("Avg frame time:     {:.3} ms", frame_time_ms);
    println!("Total collision checks: {}", total_checks);
    println!(
        "Checks per second:  {:.0}",
        total_checks as f64 / duration.as_secs_f64()
    );

    // Count entities near obstacle
    let mut count_query = Query::<(&Transform,)>::new(&mut world);
    let near_count = count_query
        .iter_mut()
        .filter(|(transform,)| {
            let dx = transform.x - 50.0;
            let dy = transform.y;
            let dist = (dx * dx + dy * dy).sqrt();
            dist < 15.0
        })
        .count();

    println!("\nEntities near obstacle: {}", near_count);
    println!("\n✓ Stress test completed!");
}
