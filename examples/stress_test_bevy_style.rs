/// Stress test using Bevy-style queries
use ecs_hybrid::*;
use std::time::Instant;

#[derive(Debug, Clone)]
struct BoxCollider {
    center: (f32, f32, f32),
    size: (f32, f32, f32),
}

impl BoxCollider {
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

fn main() {
    println!("=== Stress Test: Bevy-Style Queries ===\n");

    let scene = Scene::new();

    // Create obstacle entity with multiple box colliders
    let obstacle = scene.instantiate();
    obstacle
        .add_component(Name::new("Obstacle"))
        .add_component(Transform::new(50.0, 0.0, 0.0));

    // Add multiple box colliders to the obstacle
    let obstacle_entity = obstacle.id;
    scene.apply_commands();

    {
        let world_lock = scene.world();
        let mut world = world_lock.write();

        // Add 5 box colliders in different positions
        world.add_component(
            obstacle_entity,
            BoxCollider::new((0.0, 0.0, 0.0), (5.0, 5.0, 5.0)),
        );
        world.add_component(
            obstacle_entity,
            BoxCollider::new((6.0, 0.0, 0.0), (4.0, 4.0, 4.0)),
        );
        world.add_component(
            obstacle_entity,
            BoxCollider::new((-6.0, 0.0, 0.0), (4.0, 4.0, 4.0)),
        );
        world.add_component(
            obstacle_entity,
            BoxCollider::new((0.0, 6.0, 0.0), (3.0, 3.0, 3.0)),
        );
        world.add_component(
            obstacle_entity,
            BoxCollider::new((0.0, -6.0, 0.0), (3.0, 3.0, 3.0)),
        );
    }

    println!("✓ Created obstacle with 5 box colliders");

    // Create moving entities
    let entity_count = 10_000;
    for i in 0..entity_count {
        let entity = scene.instantiate();
        let angle = (i as f32 / entity_count as f32) * std::f32::consts::PI * 2.0;
        entity
            .add_component(Name::new(format!("Entity_{}", i)))
            .add_component(Transform::new(angle.cos() * 20.0, angle.sin() * 20.0, 0.0))
            .add_component(Velocity::new(angle.cos() * 2.0, angle.sin() * 2.0, 0.0));
    }

    scene.apply_commands();

    println!("✓ Created {} moving entities", entity_count);
    println!("\nScenario: Entities move toward obstacle with 5 box colliders");
    println!("Collision check: Bevy-style query iteration");
    println!("\nRunning 10,000 frame simulation...\n");

    let frame_count = 10_000;
    let start = Instant::now();

    // Bevy-style system execution
    for _frame in 0..frame_count {
        let world_lock = scene.world();
        let world = world_lock.read();

        // Pre-collect obstacle data once per frame (no cloning - just position values)
        let obs_pos = world
            .get_component::<Transform>(obstacle_entity)
            .map(|t| (t.x, t.y, t.z));

        // Collect collision info: (entity_id, new_position, should_collide)
        let mut collision_checks: Vec<(usize, f32, f32, f32, bool)> = Vec::new();

        // First pass: calculate new positions and check collisions (read-only)
        for (idx, (transform, velocity)) in world.query2::<Transform, Velocity>().enumerate() {
            let new_x = transform.x + velocity.x * 0.016;
            let new_y = transform.y + velocity.y * 0.016;
            let new_z = transform.z + velocity.z * 0.016;

            let mut collided = false;
            if let Some((obs_x, obs_y, obs_z)) = obs_pos {
                let obs_transform = Transform::new(obs_x, obs_y, obs_z);

                // Check collisions through closure (no clone!)
                world.with_components::<BoxCollider, _, _>(obstacle_entity, |colliders| {
                    for collider in colliders {
                        if collider.intersects(&obs_transform, (new_x, new_y, new_z)) {
                            collided = true;
                            break;
                        }
                    }
                });
            }

            collision_checks.push((idx, new_x, new_y, new_z, collided));
        }

        drop(world);

        // Second pass: apply movement (write lock)
        let mut world = world_lock.write();

        // Bevy-style: Query for mutable components
        if let Some(mut idx) = Some(0) {
            for (transform, _velocity) in world.query2_mut_mut::<Transform, Velocity>() {
                if let Some(&(_, new_x, new_y, new_z, collided)) = collision_checks.get(idx) {
                    if !collided {
                        transform.x = new_x;
                        transform.y = new_y;
                        transform.z = new_z;
                    }
                    idx += 1;
                }
            }
        }
    }

    let duration = start.elapsed();

    // Calculate results
    let fps = frame_count as f64 / duration.as_secs_f64();
    let frame_time_ms = duration.as_secs_f64() * 1000.0 / frame_count as f64;
    let total_checks = entity_count * frame_count * 5; // 5 colliders

    println!("=== Results ===");
    println!("Iteration Style:    Bevy (query)");
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

    // Count stopped entities
    let world_lock = scene.world();
    let world = world_lock.read();

    let stopped_count = world
        .query::<Transform>()
        .map(|iter| {
            iter.filter(|(entity, transform)| {
                if *entity == obstacle_entity {
                    return false;
                }
                let dx = transform.x - 50.0;
                let dy = transform.y;
                let dist = (dx * dx + dy * dy).sqrt();
                dist < 15.0 // Near obstacle
            })
            .count()
        })
        .unwrap_or(0);

    drop(world);
    drop(world_lock);

    println!("\nEntities near obstacle: {}", stopped_count);
    println!("\n✓ Stress test completed!");
}
