use crate::ecs_core::{
    BoxCollider, Component, Entity, Position, ScriptComponent, UpdateContext, World,
};

// Example components
#[derive(Debug)]
pub struct Velocity {
    pub dx: f32,
    pub dy: f32,
}

impl Component for Velocity {}

#[derive(Debug)]
pub struct Name(pub String);

impl Component for Name {}

// Script components - these have update logic
pub struct MoverScript {
    pub speed: f32,
}

impl Component for MoverScript {}

impl ScriptComponent for MoverScript {
    fn update(&mut self, entity: Entity, world: &World, ctx: &mut UpdateContext) {
        // Access and modify the entity's Position based on Velocity
        if let Some(vel) = world.get_component::<Velocity>(entity) {
            let dx = vel.dx * self.speed;
            let dy = vel.dy * self.speed;

            // Use context to schedule the position update
            ctx.move_position(entity, dx, dy, world);

            if let Some(pos) = world.get_component::<Position>(entity) {
                println!(
                    "  MoverScript updating Entity {:?}: moving from ({}, {}) by ({}, {})",
                    entity, pos.x, pos.y, dx, dy
                );
            }
        }
    }
}

// Mover script with collision detection
pub struct CollisionMoverScript {
    pub speed: f32,
}

impl Component for CollisionMoverScript {}

impl ScriptComponent for CollisionMoverScript {
    fn update(&mut self, entity: Entity, world: &World, ctx: &mut UpdateContext) {
        // Access and modify the entity's Position based on Velocity
        if let Some(vel) = world.get_component::<Velocity>(entity) {
            let dx = vel.dx * self.speed;
            let dy = vel.dy * self.speed;

            // Use context to schedule the position update WITH collision detection
            ctx.move_position_with_collision(entity, dx, dy, world);

            if let Some(pos) = world.get_component::<Position>(entity) {
                println!(
                    "  CollisionMoverScript updating Entity {:?}: moving from ({}, {}) by ({}, {})",
                    entity, pos.x, pos.y, dx, dy
                );
            }
        }
    }
}

// Silent collision mover for performance testing
pub struct SilentCollisionMoverScript {
    pub speed: f32,
}

impl Component for SilentCollisionMoverScript {}

impl ScriptComponent for SilentCollisionMoverScript {
    fn update(&mut self, entity: Entity, world: &World, ctx: &mut UpdateContext) {
        if let Some(vel) = world.get_component::<Velocity>(entity) {
            let dx = vel.dx * self.speed;
            let dy = vel.dy * self.speed;
            ctx.move_position_with_collision(entity, dx, dy, world);
        }
    }
}

pub struct LoggerScript {
    pub message: String,
}

impl Component for LoggerScript {}

impl ScriptComponent for LoggerScript {
    fn update(&mut self, entity: Entity, world: &World, _ctx: &mut UpdateContext) {
        if let Some(name) = world.get_component::<Name>(entity) {
            println!("  LoggerScript: {} - {}", name.0, self.message);
        } else {
            println!("  LoggerScript on Entity {:?}: {}", entity, self.message);
        }
    }
}

pub fn run_example() {
    println!("=== ECS-like Storage Architecture MVP ===\n");

    let mut world = World::new();

    // Create entities with different component combinations
    let player = world.create_entity();
    world.add_component(player, Name("Player".to_string()));
    world.add_component(player, Position { x: 0.0, y: 0.0 });
    world.add_component(player, Velocity { dx: 1.0, dy: 0.5 });

    let enemy = world.create_entity();
    world.add_component(enemy, Name("Enemy".to_string()));
    world.add_component(enemy, Position { x: 10.0, y: 5.0 });

    let static_object = world.create_entity();
    world.add_component(static_object, Name("Static Object".to_string()));
    world.add_component(static_object, Position { x: -5.0, y: -5.0 });

    // Query all entities with Position
    println!("Entities with Position:");
    for (entity, pos) in world.query::<Position>() {
        println!("  Entity {:?}: Position({}, {})", entity, pos.x, pos.y);
    }

    // Query all entities with both Position and Velocity
    println!("\nEntities with Position AND Velocity:");
    for (entity, pos, vel) in world.query2::<Position, Velocity>() {
        println!(
            "  Entity {:?}: Pos({}, {}), Vel({}, {})",
            entity, pos.x, pos.y, vel.dx, vel.dy
        );
    }

    // Update system: Move entities with both position and velocity
    println!("\nApplying movement system...");
    let dt = 1.0; // delta time

    // Get entities to update (we need to do this to avoid borrow checker issues)
    let entities_to_update: Vec<_> = world
        .query2::<Position, Velocity>()
        .iter()
        .map(|(e, _, _)| *e)
        .collect();

    for entity in entities_to_update {
        if let Some(vel) = world.get_component::<Velocity>(entity) {
            let dx = vel.dx * dt;
            let dy = vel.dy * dt;

            if let Some(pos) = world.get_component_mut::<Position>(entity) {
                pos.x += dx;
                pos.y += dy;
            }
        }
    }

    // Query positions again to see the changes
    println!("\nPositions after movement:");
    for (entity, pos) in world.query::<Position>() {
        if let Some(name) = world.get_component::<Name>(entity) {
            println!("  {}: Position({}, {})", name.0, pos.x, pos.y);
        }
    }

    // Remove a component
    println!("\nRemoving Velocity from player...");
    world.remove_component::<Velocity>(player);

    println!("\nEntities with Position AND Velocity after removal:");
    for (entity, pos, vel) in world.query2::<Position, Velocity>() {
        println!(
            "  Entity {:?}: Pos({}, {}), Vel({}, {})",
            entity, pos.x, pos.y, vel.dx, vel.dy
        );
    }

    println!("\n=== ECS MVP Complete ===");

    // Now demonstrate script components
    println!("\n\n=== Script Components Demo ===\n");

    let mut world2 = World::new();

    // Create entities with script components
    let scripted_entity1 = world2.create_entity();
    world2.add_component(scripted_entity1, Name("Scripted Entity 1".to_string()));
    world2.add_component(scripted_entity1, Position { x: 0.0, y: 0.0 });
    world2.add_component(scripted_entity1, Velocity { dx: 2.0, dy: 1.0 });
    world2.add_script_component(scripted_entity1, MoverScript { speed: 1.5 });
    world2.add_script_component(
        scripted_entity1,
        LoggerScript {
            message: "I'm moving!".to_string(),
        },
    );

    let scripted_entity2 = world2.create_entity();
    world2.add_component(scripted_entity2, Name("Scripted Entity 2".to_string()));
    world2.add_script_component(
        scripted_entity2,
        LoggerScript {
            message: "Just logging...".to_string(),
        },
    );

    let scripted_entity3 = world2.create_entity();
    world2.add_component(scripted_entity3, Name("Fast Mover".to_string()));
    world2.add_component(scripted_entity3, Position { x: 5.0, y: 5.0 });
    world2.add_component(scripted_entity3, Velocity { dx: 3.0, dy: -2.0 });
    world2.add_script_component(scripted_entity3, MoverScript { speed: 2.0 });

    println!("Running update loop (3 frames)...\n");

    for frame in 1..=3 {
        println!("Frame {}:", frame);
        world2.update_scripts();

        // Print actual positions after update
        println!("  Positions after update:");
        for (entity, pos) in world2.query::<Position>() {
            if let Some(name) = world2.get_component::<Name>(entity) {
                println!("    {}: ({}, {})", name.0, pos.x, pos.y);
            }
        }
        println!();
    }

    println!("=== Script Components Demo Complete ===");
}

pub fn run_rendering_example() {
    use crate::ecs_core::Sprite;
    use crate::renderer::Renderer;

    println!("\n=== Sprite Rendering Demo with Collision ===\n");

    // Create renderer first (this initializes the window)
    let renderer = Renderer::new(800.0, 600.0);

    let mut world = World::new();

    // Create collision walls (5 box colliders)
    let wall_top = world.create_entity();
    world.add_component(wall_top, Name("Wall Top".to_string()));
    world.add_component(wall_top, Position { x: 0.0, y: 200.0 });
    world.add_component(wall_top, BoxCollider::new(400.0, 40.0));
    world.add_component(wall_top, Sprite::new((0.3, 0.3, 0.3), 400.0, 40.0));

    let wall_bottom = world.create_entity();
    world.add_component(wall_bottom, Name("Wall Bottom".to_string()));
    world.add_component(wall_bottom, Position { x: 0.0, y: -200.0 });
    world.add_component(wall_bottom, BoxCollider::new(400.0, 40.0));
    world.add_component(wall_bottom, Sprite::new((0.3, 0.3, 0.3), 400.0, 40.0));

    let wall_left = world.create_entity();
    world.add_component(wall_left, Name("Wall Left".to_string()));
    world.add_component(wall_left, Position { x: -200.0, y: 0.0 });
    world.add_component(wall_left, BoxCollider::new(40.0, 400.0));
    world.add_component(wall_left, Sprite::new((0.3, 0.3, 0.3), 40.0, 400.0));

    let wall_right = world.create_entity();
    world.add_component(wall_right, Name("Wall Right".to_string()));
    world.add_component(wall_right, Position { x: 200.0, y: 0.0 });
    world.add_component(wall_right, BoxCollider::new(40.0, 400.0));
    world.add_component(wall_right, Sprite::new((0.3, 0.3, 0.3), 40.0, 400.0));

    let wall_center = world.create_entity();
    world.add_component(wall_center, Name("Wall Center".to_string()));
    world.add_component(wall_center, Position { x: 0.0, y: 0.0 });
    world.add_component(wall_center, BoxCollider::new(60.0, 60.0));
    world.add_component(wall_center, Sprite::new((0.5, 0.5, 0.5), 60.0, 60.0));

    // Create colored entities with sprites that collide with walls
    let red_mover = world.create_entity();
    world.add_component(red_mover, Name("Red Mover".to_string()));
    world.add_component(red_mover, Position { x: -100.0, y: 0.0 });
    world.add_component(red_mover, Velocity { dx: 2.0, dy: 1.0 });
    world.add_component(red_mover, Sprite::new((1.0, 0.2, 0.2), 30.0, 30.0));
    world.add_script_component(red_mover, CollisionMoverScript { speed: 1.0 });

    let blue_mover = world.create_entity();
    world.add_component(blue_mover, Name("Blue Mover".to_string()));
    world.add_component(blue_mover, Position { x: 100.0, y: 50.0 });
    world.add_component(blue_mover, Velocity { dx: -1.5, dy: 0.5 });
    world.add_component(blue_mover, Sprite::new((0.2, 0.4, 1.0), 35.0, 35.0));
    world.add_script_component(blue_mover, CollisionMoverScript { speed: 1.0 });

    let green_bouncer = world.create_entity();
    world.add_component(green_bouncer, Name("Green Bouncer".to_string()));
    world.add_component(green_bouncer, Position { x: 0.0, y: -80.0 });
    world.add_component(green_bouncer, Velocity { dx: 1.0, dy: 2.0 });
    world.add_component(green_bouncer, Sprite::new((0.2, 1.0, 0.3), 25.0, 25.0));
    world.add_script_component(green_bouncer, CollisionMoverScript { speed: 1.5 });

    let purple_circle = world.create_entity();
    world.add_component(purple_circle, Name("Purple Circle".to_string()));
    world.add_component(
        purple_circle,
        Position {
            x: -150.0,
            y: 100.0,
        },
    );
    world.add_component(purple_circle, Velocity { dx: 0.5, dy: -1.0 });
    world.add_component(purple_circle, Sprite::new((0.8, 0.2, 0.9), 28.0, 28.0));
    world.add_script_component(purple_circle, CollisionMoverScript { speed: 1.2 });

    println!("Created 9 entities:");
    println!("- 5 gray box colliders (walls + center obstacle)");
    println!("- 4 colored movers with collision detection");
    println!("\nRendering loop running... (press Ctrl+C to exit)\n");

    // Main rendering loop
    let mut frame = 0;
    loop {
        frame += 1;

        // Update all script components
        world.update_scripts();

        // Render the world
        renderer.clear();
        renderer.render(&world);
        renderer.present();

        // Print debug info every 2 frames (~1 second at 0.5s per frame)
        if frame % 20 == 0 {
            println!("Frame {}: Entities rendered", frame);
            for (entity, pos) in world.query::<Position>() {
                if let Some(name) = world.get_component::<Name>(entity) {
                    println!("  {}: ({:.1}, {:.1})", name.0, pos.x, pos.y);
                }
            }
            println!();
        }

        // Wait 0.5 seconds between frames
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

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
            world.add_script_component(entity, SilentCollisionMoverScript { speed: 1.0 });
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

// Profiling test to find bottlenecks
pub fn run_bottleneck_analysis() {
    use std::time::Instant;

    println!("\n=== ECS Bottleneck Analysis ===\n");

    let num_movers = 1000;
    let num_colliders = 5;

    println!("Test Configuration:");
    println!("  Movers: {}", num_movers);
    println!("  Colliders: {}", num_colliders);
    println!(
        "  Collision checks per frame: {} × {} = {}",
        num_movers,
        num_colliders,
        num_movers * num_colliders
    );
    println!("\nRunning detailed profiling...\n");

    let mut world = World::new();

    // Store entity IDs for later reference
    let mut mover_entities = Vec::new();

    // Create collision walls
    for i in 0..num_colliders {
        let entity = world.create_entity();
        world.add_component(entity, Name(format!("Collider {}", i)));
        world.add_component(
            entity,
            Position {
                x: (i as f32 * 100.0) - 200.0,
                y: 0.0,
            },
        );
        world.add_component(entity, BoxCollider::new(40.0, 400.0));
    }

    // Create moving entities
    for i in 0..num_movers {
        let entity = world.create_entity();
        mover_entities.push(entity);
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
        world.add_script_component(entity, SilentCollisionMoverScript { speed: 1.0 });
    }

    // Warmup
    for _ in 0..10 {
        world.update_scripts();
    }

    // Profile individual operations
    let frames = 100;

    // Test 1: Just script iteration overhead
    println!("Test 1: Measuring query overhead...");
    let start = Instant::now();
    for _ in 0..frames {
        let _ = world.query::<Position>();
    }
    let query_time = start.elapsed().as_secs_f64() * 1000.0 / frames as f64;
    println!("  Query overhead: {:.4} ms/frame\n", query_time);

    // Test 2: Full update with collision detection
    println!("Test 2: Full update_scripts() with collision detection...");
    let start = Instant::now();
    for _ in 0..frames {
        world.update_scripts();
    }
    let full_update_time = start.elapsed().as_secs_f64() * 1000.0 / frames as f64;
    println!("  Full update: {:.4} ms/frame\n", full_update_time);

    // Test 3: Count collision checks manually
    println!("Test 3: Analyzing collision detection cost...");
    let collider_query_start = Instant::now();
    let mut total_checks = 0;
    for _ in 0..frames {
        let colliders = world.query2::<Position, BoxCollider>();
        total_checks += num_movers * colliders.len();
    }
    let collider_query_time = collider_query_start.elapsed().as_secs_f64() * 1000.0 / frames as f64;
    let checks_per_frame = total_checks / frames;
    println!("  Collision checks per frame: {}", checks_per_frame);
    println!("  Collider query time: {:.4} ms/frame", collider_query_time);
    println!(
        "  Time per collision check: {:.6} ms\n",
        full_update_time / checks_per_frame as f64
    );

    // Test 4: Measure position updates (mutation application)
    println!("Test 4: Component mutation overhead...");
    let mutation_start = Instant::now();
    for _ in 0..frames {
        for &entity in &mover_entities {
            if let Some(pos) = world.get_component_mut::<Position>(entity) {
                pos.x += 1.0;
                pos.y += 1.0;
            }
        }
    }
    let mutation_time = mutation_start.elapsed().as_secs_f64() * 1000.0 / frames as f64;
    println!("  Mutation time: {:.4} ms/frame\n", mutation_time);

    // Summary
    println!("=== Bottleneck Analysis ===");
    println!("Per-frame breakdown:");
    println!(
        "  1. Query overhead:          {:.4} ms ({:.1}%)",
        query_time,
        (query_time / full_update_time) * 100.0
    );
    println!(
        "  2. Collision detection:     {:.4} ms ({:.1}%)",
        full_update_time - mutation_time,
        ((full_update_time - mutation_time) / full_update_time) * 100.0
    );
    println!(
        "  3. Mutation application:    {:.4} ms ({:.1}%)",
        mutation_time,
        (mutation_time / full_update_time) * 100.0
    );
    println!("  4. TOTAL:                   {:.4} ms", full_update_time);

    println!("\n=== Recommendations ===");
    let collision_percent = ((full_update_time - mutation_time) / full_update_time) * 100.0;
    if collision_percent > 60.0 {
        println!(
            "⚠ Collision detection is the bottleneck ({:.1}%)",
            collision_percent
        );
        println!("  Suggested optimizations:");
        println!("  - Implement spatial partitioning (quadtree/grid)");
        println!("  - Cache nearby colliders");
        println!("  - Use broad-phase collision detection");
    } else if (mutation_time / full_update_time) * 100.0 > 60.0 {
        println!("⚠ Component mutations are the bottleneck");
        println!("  Suggested optimizations:");
        println!("  - Use arena allocators");
        println!("  - Batch mutations");
        println!("  - Consider archetype-based storage");
    } else {
        println!("✓ Performance is well-balanced");
        println!("  Consider parallel processing for further gains");
    }

    println!("\n=== Analysis Complete ===\n");
}
