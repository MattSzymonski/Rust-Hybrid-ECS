// Hybrid ECS Game Engine Demo
// Combines ECS performance with Unity-like object-oriented API

use ecs_hybrid::*;

fn main() {
    println!("=== Hybrid ECS Game Engine Demo ===\n");

    // Create a scene (like Unity)
    let scene = Scene::new();

    println!("--- Unity-like Object Creation ---");

    // Create GameObjects with Unity-like API
    let player = scene.instantiate();
    player
        .add_component(Name::new("Player"))
        .add_component(Transform::new(0.0, 0.0, 0.0))
        .add_component(Velocity::new(1.0, 0.5, 0.0))
        .add_component(Health::new(100.0));

    println!("✓ Created player GameObject");

    let enemy = scene.instantiate();
    enemy
        .add_component(Name::new("Enemy"))
        .add_component(Transform::new(10.0, 5.0, 0.0))
        .add_component(Velocity::new(-0.5, 0.0, 0.0))
        .add_component(Health::new(50.0));

    enemy.get_component_mut::<Health>().unwrap().with(|health| {
        health.current = 75.0; // Modify health immediately
    });

    let (current_health, max_health) = enemy
        .get_component::<Health>()
        .unwrap()
        .with(|health: &Health| (health.current, health.max))
        .unwrap();

    println!("Enemy health: {}/{}", current_health, max_health);

    println!("✓ Created enemy GameObject");

    let static_object = scene.instantiate();
    static_object
        .add_component(Name::new("Wall"))
        .add_component(Transform::new(5.0, 0.0, 0.0));

    println!("✓ Created static object (no velocity)");

    // Apply any pending commands
    scene.apply_commands();

    println!("\n--- Reading Component Data (Unity-like) ---");

    // Access components Unity-style
    if let Some(name) = player.get_component::<Name>() {
        name.with(|n| {
            println!("Player name: {}", n.value);
        });
    }

    //  // Use a macro to simplify component access
    //     macro_rules! with_component {
    //         ($entity:expr, $component:ty, $closure:expr) => {
    //             if let Some(comp) = $entity.get_component::<$component>() {
    //                 comp.with($closure);
    //             }
    //         };
    //     }

    //     with_component!(player, Transform, |t| {
    //         println!("Player position: ({}, {}, {})", t.x, t.y, t.z);
    //     });

    if let Some(transform) = player.get_component::<Transform>() {
        transform.with(|t| {
            println!("Player position: ({}, {}, {})", t.x, t.y, t.z);
        });
    }

    println!("\n--- ECS System Execution (Parallel-friendly) ---");

    // Create system executor
    let mut executor = SystemExecutor::new();
    executor.add_system(MovementSystem);

    // Simulate multiple frames
    for frame in 1..=3 {
        println!("\nFrame {}:", frame);

        // Execute systems (can run in parallel in real implementation)
        {
            let world_lock = scene.world();
            let mut world = world_lock.write();
            executor.execute(&mut world, 0.016); // ~60 FPS
        }

        // Query and display positions (demonstrating ECS iteration)
        let world_lock = scene.world();
        let world = world_lock.read();

        // Collect data to avoid lifetime issues
        let positions: Vec<(String, f32, f32, f32)> = world
            .query::<Transform>()
            .map(|query| {
                query
                    .filter_map(|(entity, transform)| {
                        world
                            .get_component::<Name>(entity)
                            .map(|name| (name.value.clone(), transform.x, transform.y, transform.z))
                    })
                    .collect()
            })
            .unwrap_or_default();

        drop(world);
        drop(world_lock);

        for (name, x, y, z) in positions {
            println!("  {} at ({:.2}, {:.2}, {:.2})", name, x, y, z);
        }
    }

    println!("\n--- Modifying Components (Unity-like) ---");

    // Modify health Unity-style
    if let Some(mut health) = player.get_component_mut::<Health>() {
        health.with(|h| {
            h.current -= 25.0;
            println!("Player took damage! Health: {}/{}", h.current, h.max);
        });
    }

    println!("\n--- Dynamic Entity Creation During Gameplay ---");

    // This is where the hybrid approach shines:
    // Create entities naturally (Unity-like), but they're actually buffered
    let projectile = scene.instantiate();
    projectile
        .add_component(Name::new("Projectile"))
        .add_component(Transform::new(0.0, 0.0, 0.0))
        .add_component(Velocity::new(5.0, 0.0, 0.0));

    println!("✓ Created projectile (will be available next frame)");

    // Apply commands at frame boundary
    scene.apply_commands();
    println!("✓ Commands applied - projectile now accessible");

    println!("\n--- Destroying GameObjects (Unity-like) ---");

    // Destroy enemy
    enemy.destroy();
    println!("✓ Queued enemy for destruction");

    scene.apply_commands();
    println!("✓ Enemy destroyed");

    // Verify enemy is gone
    let world_lock = scene.world();
    let world = world_lock.read();
    let remaining: Vec<String> = world
        .query::<Name>()
        .map(|query| query.map(|(_, name)| name.value.clone()).collect())
        .unwrap_or_default();
    drop(world);
    drop(world_lock);

    println!("\nRemaining entities:");
    for name in remaining {
        println!("  - {}", name);
    }

    println!("\n=== Comparison Table ===");
    println!("\n┌─────────────────┬──────────────────────┬──────────────────────┬──────────────────────┐");
    println!(
        "│ Feature         │ Unity                │ Bevy (Pure ECS)      │ This Hybrid          │"
    );
    println!(
        "├─────────────────┼──────────────────────┼──────────────────────┼──────────────────────┤"
    );
    println!(
        "│ API Style       │ OOP, Immediate       │ ECS, Deferred        │ OOP Wrapper + ECS    │"
    );
    println!(
        "│ Create Entity   │ Instantiate()        │ commands.spawn()     │ scene.instantiate()  │"
    );
    println!(
        "│ Add Component   │ AddComponent<T>()    │ .insert(Component)   │ .add_component(T)    │"
    );
    println!(
        "│ Access Comp     │ GetComponent<T>()    │ Query<&T>            │ .get_component<T>()  │"
    );
    println!(
        "│ Parallel Systems│ ✗ Single-threaded    │ ✓ Automatic          │ ✓ Manual/Rayon       │"
    );
    println!(
        "│ Intuitive       │ ✓✓ Very natural      │ △ Learning curve     │ ✓ Natural for Unity  │"
    );
    println!(
        "│ Performance     │ △ Limited            │ ✓✓ Excellent         │ ✓ Good (ECS backend) │"
    );
    println!(
        "│ Thread Safety   │ △ Manual locking     │ ✓✓ Automatic         │ ✓ CommandBuffer      │"
    );
    println!(
        "│ State Sync      │ ✓ Always consistent  │ △ Frame delay        │ ✓ Controlled delay   │"
    );
    println!(
        "└─────────────────┴──────────────────────┴──────────────────────┴──────────────────────┘"
    );

    println!("\n=== Key Insights ===");
    println!("1. Unity API: Natural but single-threaded, immediate changes");
    println!("2. Bevy ECS: High performance but deferred operations can be confusing");
    println!("3. Hybrid: Best of both - Unity-like API with ECS performance");
    println!("4. Command Buffer: Solves the 'inconsistent state' problem");
    println!("5. GameObject Wrapper: Hides ECS complexity from game developers");
    println!("\n✓ Demo completed successfully!");
    println!("\nRun 'cargo run --example performance_test' for comprehensive benchmarks!");
}
