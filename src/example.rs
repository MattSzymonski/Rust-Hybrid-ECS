use crate::{
    components::{BoxCollider, MoverScript, Name, Position, Velocity},
    ecs_core::{Component, World},
};
use rayon::prelude::*;

trait_type_map::impl_trait_accessible!(dyn Component; Position, Velocity, Name, BoxCollider, MoverScript);

pub fn run_performance_test_scripts() {
    use std::time::Instant;

    println!("\n=== Scripts ECS Performance Test ===\n");
    println!("Running 10 tests with increasing entity counts...\n");
    println!("Each test: 100 warmup frames + 60 timed frames\n");
    println!(
        "{:<10} {:<15} {:<15} {:<15} {:<15}",
        "Entities", "Test Time (s)", "Avg FPS", "Avg Frame (ms)", "Total+Warmup (s)"
    );
    println!("{}", "-".repeat(75));

    for run in 1..=10 {
        let num_movers = run * 500;
        let mut world = World::new();

        world.register_component_type::<Name>();
        world.register_component_type::<Position>();
        world.register_component_type::<BoxCollider>();
        world.register_component_type::<MoverScript>();
        world.register_component_type::<Velocity>();

        // Create collision walls (5 box colliders)
        let wall_top = world.create_entity();
        world.add_component(wall_top, Name("Wall Top".to_string()));
        world.add_component(wall_top, Position { x: 0.0, y: 200.0 });
        world.add_component(wall_top, BoxCollider::new(400.0, 40.0));

        let wall_bottom = world.create_entity();
        world.add_component(wall_bottom, Name("Wall Bottom".to_string()));
        world.add_component(wall_bottom, Position { x: 0.0, y: -200.0 });
        world.add_component(wall_bottom, BoxCollider::new(400.0, 40.0));

        let wall_left = world.create_entity();
        world.add_component(wall_left, Name("Wall Left".to_string()));
        world.add_component(wall_left, Position { x: -200.0, y: 0.0 });
        world.add_component(wall_left, BoxCollider::new(40.0, 400.0));

        let wall_right = world.create_entity();
        world.add_component(wall_right, Name("Wall Right".to_string()));
        world.add_component(wall_right, Position { x: 200.0, y: 0.0 });
        world.add_component(wall_right, BoxCollider::new(40.0, 400.0));

        let wall_center = world.create_entity();
        world.add_component(wall_center, Name("Wall Center".to_string()));
        world.add_component(wall_center, Position { x: 0.0, y: 0.0 });
        world.add_component(wall_center, BoxCollider::new(60.0, 60.0));

        // Create multiple moving entities with collision detection
        for i in 0..num_movers {
            let entity = world.create_entity();
            world.add_component(entity, Name(format!("Mover {}", i)));
            world.add_component(
                entity,
                Position {
                    x: (i as f32 * 10.0) - 100.0,
                    y: (i as f32 * 5.0) - 50.0,
                },
            );
            world.add_component(
                entity,
                Velocity {
                    dx: ((i % 3) as f32 - 1.0) * 2.0,
                    dy: ((i % 5) as f32 - 2.0) * 1.5,
                },
            );
            world.add_component(entity, MoverScript { speed: 1.0 });
        }

        // Warmup - run 100 frames without timing
        let warmup_start = Instant::now();
        for _ in 0..100 {
            world.update_scripts();
        }
        let warmup_duration = warmup_start.elapsed();

        // Performance test - 60 frames
        let total_frames = 60;
        let test_start = Instant::now();

        for _ in 0..total_frames {
            world.update_scripts();
        }

        let total_duration = test_start.elapsed();
        let total_entities = num_movers + 5; // movers + walls
        let avg_fps = total_frames as f64 / total_duration.as_secs_f64();
        let avg_frame_ms = (total_duration.as_secs_f64() * 1000.0) / total_frames as f64;
        let total_time_with_warmup = warmup_duration.as_secs_f64() + total_duration.as_secs_f64();

        println!(
            "{:<10} {:<15.4} {:<15.2} {:<15.4} {:<15.4}",
            total_entities,
            total_duration.as_secs_f64(),
            avg_fps,
            avg_frame_ms,
            total_time_with_warmup
        );
    }

    println!("\n=== Test Complete ===\n");
}

pub fn run_performance_test_systems() {
    use std::time::Instant;

    println!("\n=== Systems ECS Performance Test ===\n");
    println!("Running 10 tests with increasing entity counts...\n");
    println!("Each test: 100 warmup frames + 60 timed frames\n");
    println!(
        "{:<10} {:<15} {:<15} {:<15} {:<15}",
        "Entities", "Test Time (s)", "Avg FPS", "Avg Frame (ms)", "Total+Warmup (s)"
    );
    println!("{}", "-".repeat(75));

    for run in 1..=10 {
        let num_movers = run * 500;
        let mut world = World::new();

        world.register_component_type::<Name>();
        world.register_component_type::<Position>();
        world.register_component_type::<BoxCollider>();
        world.register_component_type::<Velocity>();

        // Create collision walls (5 box colliders)
        let wall_top = world.create_entity();
        world.add_component(wall_top, Name("Wall Top".to_string()));
        world.add_component(wall_top, Position { x: 0.0, y: 200.0 });
        world.add_component(wall_top, BoxCollider::new(400.0, 40.0));

        let wall_bottom = world.create_entity();
        world.add_component(wall_bottom, Name("Wall Bottom".to_string()));
        world.add_component(wall_bottom, Position { x: 0.0, y: -200.0 });
        world.add_component(wall_bottom, BoxCollider::new(400.0, 40.0));

        let wall_left = world.create_entity();
        world.add_component(wall_left, Name("Wall Left".to_string()));
        world.add_component(wall_left, Position { x: -200.0, y: 0.0 });
        world.add_component(wall_left, BoxCollider::new(40.0, 400.0));

        let wall_right = world.create_entity();
        world.add_component(wall_right, Name("Wall Right".to_string()));
        world.add_component(wall_right, Position { x: 200.0, y: 0.0 });
        world.add_component(wall_right, BoxCollider::new(40.0, 400.0));

        let wall_center = world.create_entity();
        world.add_component(wall_center, Name("Wall Center".to_string()));
        world.add_component(wall_center, Position { x: 0.0, y: 0.0 });
        world.add_component(wall_center, BoxCollider::new(60.0, 60.0));

        // Create multiple moving entities with velocity (no scripts!)
        for i in 0..num_movers {
            let entity = world.create_entity();
            world.add_component(entity, Name(format!("Mover {}", i)));
            world.add_component(
                entity,
                Position {
                    x: (i as f32 * 10.0) - 100.0,
                    y: (i as f32 * 5.0) - 50.0,
                },
            );
            world.add_component(
                entity,
                Velocity {
                    dx: ((i % 3) as f32 - 1.0) * 2.0,
                    dy: ((i % 5) as f32 - 2.0) * 1.5,
                },
            );
        }

        // Warmup - run 100 frames without timing
        let warmup_start = Instant::now();
        for _ in 0..100 {
            //movement_system(&mut world);
            movement_system_parallelized(&mut world);
        }
        let warmup_duration = warmup_start.elapsed();

        // Performance test - 60 frames
        let total_frames = 60;
        let test_start = Instant::now();

        for _ in 0..total_frames {
            // movement_system(&mut world);
            movement_system_parallelized(&mut world);
        }

        let total_duration = test_start.elapsed();
        let total_entities = num_movers + 5; // movers + walls
        let avg_fps = total_frames as f64 / total_duration.as_secs_f64();
        let avg_frame_ms = (total_duration.as_secs_f64() * 1000.0) / total_frames as f64;
        let total_time_with_warmup = warmup_duration.as_secs_f64() + total_duration.as_secs_f64();

        println!(
            "{:<10} {:<15.4} {:<15.2} {:<15.4} {:<15.4}",
            total_entities,
            total_duration.as_secs_f64(),
            avg_fps,
            avg_frame_ms,
            total_time_with_warmup
        );
    }

    println!("\n=== Test Complete ===\n");
}

// Pure ECS system - processes all entities with Position and Velocity
fn movement_system(world: &mut World) {
    // Collect all entities with Position and Velocity (movers)
    let mut updates = Vec::new();

    for (entity, pos, vel) in world.get_two_component_iterator::<Position, Velocity>() {
        let dx = vel.dx;
        let dy = vel.dy;
        let mut new_x = pos.x + dx;
        let mut new_y = pos.y + dy;

        // Check collision against all colliders
        for (_collider_entity, collider_pos, collider) in
            world.get_two_component_iterator::<Position, BoxCollider>()
        {
            // Create a temporary collider for the moving entity (assume small size)
            let mover_collider = BoxCollider::new(10.0, 10.0);
            let test_pos = Position { x: new_x, y: new_y };

            // Check if the new position would collide
            if mover_collider.overlaps(&test_pos, collider, collider_pos) {
                // Collision detected - clamp to collider edge
                let half_width = mover_collider.width / 2.0;
                let half_height = mover_collider.height / 2.0;
                let c_half_width = collider.width / 2.0;
                let c_half_height = collider.height / 2.0;

                // Calculate overlap on each axis
                let overlap_left = (collider_pos.x - c_half_width) - (new_x + half_width);
                let overlap_right = (new_x - half_width) - (collider_pos.x + c_half_width);
                let overlap_bottom = (collider_pos.y - c_half_height) - (new_y + half_height);
                let overlap_top = (new_y - half_height) - (collider_pos.y + c_half_height);

                // Find the smallest overlap to determine collision direction
                let min_overlap_x = if overlap_left.abs() < overlap_right.abs() {
                    overlap_left
                } else {
                    overlap_right
                };
                let min_overlap_y = if overlap_bottom.abs() < overlap_top.abs() {
                    overlap_bottom
                } else {
                    overlap_top
                };

                // Clamp position to collider edge
                if min_overlap_x.abs() < min_overlap_y.abs() {
                    new_x += min_overlap_x;
                } else {
                    new_y += min_overlap_y;
                }
            }
        }

        // Store the update for this entity
        updates.push((entity, new_x, new_y));
    }

    // Apply all position updates
    for (entity, new_x, new_y) in updates {
        if let Some(pos) = world.get_component_mut::<Position>(entity) {
            pos.x = new_x;
            pos.y = new_y;
        }
    }
}

// Pure ECS system - processes all entities with Position and Velocity
fn movement_system_parallelized(world: &mut World) {
    // Step 1: Collect all entities with Position and Velocity into a Vec
    let movers: Vec<_> = world
        .get_two_component_iterator::<Position, Velocity>()
        .map(|(entity, pos, vel)| (entity, pos.x, pos.y, vel.dx, vel.dy))
        .collect();

    // Step 2: Collect all colliders into a Vec (so we can share them across threads)
    let colliders: Vec<_> = world
        .get_two_component_iterator::<Position, BoxCollider>()
        .map(|(entity, pos, collider)| (entity, pos.x, pos.y, collider.width, collider.height))
        .collect();

    // Step 3: Process movement calculations in parallel
    let updates: Vec<_> = movers
        .par_iter()
        .map(|(entity, pos_x, pos_y, dx, dy)| {
            let mut new_x = pos_x + dx;
            let mut new_y = pos_y + dy;

            // Check collision against all colliders
            for (_collider_entity, c_x, c_y, c_width, c_height) in &colliders {
                // Create a temporary collider for the moving entity (assume small size)
                let mover_width = 10.0;
                let mover_height = 10.0;

                // Check if the new position would collide
                let half_width1 = mover_width / 2.0;
                let half_height1 = mover_height / 2.0;
                let half_width2 = c_width / 2.0;
                let half_height2 = c_height / 2.0;

                let left1 = new_x - half_width1;
                let right1 = new_x + half_width1;
                let top1 = new_y + half_height1;
                let bottom1 = new_y - half_height1;

                let left2 = c_x - half_width2;
                let right2 = c_x + half_width2;
                let top2 = c_y + half_height2;
                let bottom2 = c_y - half_height2;

                let overlaps =
                    !(right1 < left2 || left1 > right2 || top1 < bottom2 || bottom1 > top2);

                if overlaps {
                    // Collision detected - clamp to collider edge
                    let overlap_left = (c_x - half_width2) - (new_x + half_width1);
                    let overlap_right = (new_x - half_width1) - (c_x + half_width2);
                    let overlap_bottom = (c_y - half_height2) - (new_y + half_height1);
                    let overlap_top = (new_y - half_height1) - (c_y + half_height2);

                    // Find the smallest overlap to determine collision direction
                    let min_overlap_x = if overlap_left.abs() < overlap_right.abs() {
                        overlap_left
                    } else {
                        overlap_right
                    };
                    let min_overlap_y = if overlap_bottom.abs() < overlap_top.abs() {
                        overlap_bottom
                    } else {
                        overlap_top
                    };

                    // Clamp position to collider edge
                    if min_overlap_x.abs() < min_overlap_y.abs() {
                        new_x += min_overlap_x;
                    } else {
                        new_y += min_overlap_y;
                    }
                }
            }

            (*entity, new_x, new_y)
        })
        .collect();

    // Step 4: Apply all position updates sequentially (to avoid data races)
    for (entity, new_x, new_y) in updates {
        if let Some(pos) = world.get_component_mut::<Position>(entity) {
            pos.x = new_x;
            pos.y = new_y;
        }
    }
}
