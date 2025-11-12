/// Stress test using Unity-style iteration (get_component)
use ecs_hybrid::*;
use parking_lot::MappedRwLockReadGuard;
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
    println!("=== Stress Test: Unity-Style Iteration ===\n");

    let scene = Scene::new();

    // Create obstacle entity with multiple box colliders
    let obstacle_entity = scene.instantiate();
    obstacle_entity
        .add_component(Name::new("Obstacle"))
        .add_component(Transform::new(50.0, 0.0, 0.0));

    // Add multiple box colliders to the obstacle
    let obstacle_entity_id = obstacle_entity.id;
    scene.apply_commands();

    obstacle_entity.add_component(BoxCollider::new((0.0, 0.0, 0.0), (5.0, 5.0, 5.0)));
    obstacle_entity.add_component(BoxCollider::new((6.0, 0.0, 0.0), (4.0, 4.0, 4.0)));
    obstacle_entity.add_component(BoxCollider::new((-6.0, 0.0, 0.0), (4.0, 4.0, 4.0)));
    obstacle_entity.add_component(BoxCollider::new((0.0, 6.0, 0.0), (3.0, 3.0, 3.0)));
    obstacle_entity.add_component(BoxCollider::new((0.0, -6.0, 0.0), (3.0, 3.0, 3.0)));

    println!("✓ Created obstacle with 5 box colliders");

    // Create moving entities
    let entity_count = 10_000;
    for i in 0..entity_count {
        let moving_entity = scene.instantiate();
        let angle = (i as f32 / entity_count as f32) * std::f32::consts::PI * 2.0;
        moving_entity
            .add_component(Name::new(format!("Entity_{}", i)))
            .add_component(Transform::new(angle.cos() * 20.0, angle.sin() * 20.0, 0.0))
            .add_component(Velocity::new(angle.cos() * 2.0, angle.sin() * 2.0, 0.0));
    }

    scene.apply_commands();

    println!("✓ Created {} moving entities", entity_count);
    println!("\nScenario: Entities move toward obstacle with 5 box colliders");
    println!("Collision check: Unity-style iteration with get_component");
    println!("\nRunning 10,000 frame simulation...\n");

    let frame_count = 10_000;
    let start = Instant::now();

    // Unity-style system execution
    for _frame in 0..frame_count {
        let world = scene.get_world().unwrap();

        // let world_lock = scene.world();
        // let world = world_lock.read();

        // Get all entity IDs (Unity-style: iterate entities, then get components)
        let entity_ids: Vec<u64> = world.entities().collect();

        // Pre-fetch obstacle transform position (copy just 3 floats, not cloning component)
        let obs_pos = world
            .get_component::<Transform>(obstacle_entity_id)
            .map(|t| (t.x, t.y, t.z));

        drop(world);
        //  drop(world_lock);

        // Process each entity using Unity-style Entity API
        for entity_id in entity_ids {
            if entity_id == obstacle_entity_id {
                continue;
            }

            let entity = scene.get_entity(entity_id);

            // Unity-style: Get velocity (read-only), then get transform (mutable)
            // Store velocity values to avoid borrow issues
            let (vx, vy, vz) = if let Some(velocity_ref) = entity.get_component::<Velocity>() {
                if let Some(vel) = velocity_ref.with(|v| (v.x, v.y, v.z)) {
                    vel
                } else {
                    continue;
                }
            } else {
                continue;
            };

            // let entity = scene.get_entity(entity_id);
            // let transform = entity.get_component_raw::<Transform>().unwrap();
            // println!("Entity position: {:?}", transform.x);

            let transform = entity.get_component_raw::<Transform>().unwrap();

            let mut transform_mut = entity.get_component_raw_mut::<Transform>().unwrap();

            if let Some(pos) = entity.with_component::<Transform, _>(|t| (t.x, t.y, t.z)) {
                println!("Entity position: {:?}", pos.0);
            }

            // Calculate new position
            let current_pos = entity
                .get_component::<Transform>()
                .and_then(|t| t.with(|tr| (tr.x, tr.y, tr.z)))
                .unwrap_or((0.0, 0.0, 0.0));

            let new_x = current_pos.0 + vx * 0.016;
            let new_y = current_pos.1 + vy * 0.016;
            let new_z = current_pos.2 + vz * 0.016;

            // Check collision with obstacle - NO CLONING using with_components
            let mut collided = false;
            if let Some((obs_x, obs_y, obs_z)) = obs_pos {
                let obs_transform = Transform::new(obs_x, obs_y, obs_z);

                // Access all colliders through closure (no clone!)
                obstacle_entity.with_components::<BoxCollider, _, _>(|colliders| {
                    for collider in colliders {
                        if collider.intersects(&obs_transform, (new_x, new_y, new_z)) {
                            collided = true;
                            break;
                        }
                    }
                });
            }

            // Apply movement if not colliding
            if !collided {
                if let Some(mut transform_ref) = entity.get_component_mut::<Transform>() {
                    transform_ref.with(|transform| {
                        transform.x = new_x;
                        transform.y = new_y;
                        transform.z = new_z;
                    });
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
    println!("Iteration Style:    Unity (get_component)");
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
                if *entity == obstacle_entity_id {
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
