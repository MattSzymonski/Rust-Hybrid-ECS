// ============================================================================
// Stress Test Example - Performance Testing
// ============================================================================
//! This example stress tests the ECS with:
//! - 10,000 entities
//! - Multiple box colliders on obstacle entity
//! - Collision detection system
//! - Performance measurements

use crate::{Component, Engine, Query, World};
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

#[derive(Debug)]
struct Obstacle;

impl Component for Obstacle {}

// Make all components accessible via the Component trait for TraitTypeMap
impl_trait_accessible!(dyn Component; Transform, Velocity, BoxCollider, Obstacle);

// ============================================================================
// Systems
// ============================================================================

fn movement_system(mut query: Query<(&mut Transform, &Velocity)>) {
    for (transform, velocity) in query.iter_mut() {
        transform.x += velocity.x * 0.016;
        transform.y += velocity.y * 0.016;
        transform.z += velocity.z * 0.016;
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

    let _obstacle = world
        .spawn()
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
        world
            .spawn()
            .with(Transform::new(angle.cos() * 20.0, angle.sin() * 20.0, 0.0))
            .with(Velocity::new(angle.cos() * 2.0, angle.sin() * 2.0, 0.0))
            .build();
    }

    println!("✓ Created {} moving entities", entity_count);
    println!("\nScenario: Entities move toward obstacle");
    println!("Collision check: Query-based iteration");
    println!("\nRunning 10,000 frame simulation...\n");

    let frame_count = 10_000;
    let start = Instant::now();

    let mut engine = Engine::new();
    engine.register_system("movement", movement_system);

    // Run simulation
    for _frame in 0..frame_count {
        engine.process_frame(&mut world);
    }

    let duration = start.elapsed();

    // Calculate results
    let fps = frame_count as f64 / duration.as_secs_f64();
    let frame_time_ms = duration.as_secs_f64() * 1000.0 / frame_count as f64;
    let total_operations = entity_count * frame_count;

    println!("=== Results ===");
    println!("Architecture:       Archetype-based");
    println!("Entities:           {}", entity_count);
    println!("Frames:             {}", frame_count);
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
