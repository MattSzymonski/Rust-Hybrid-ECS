use crate::{
    components::{BoxCollider, MoverScript, Name, Position, Velocity},
    ecs_core::{Component, World},
};

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
