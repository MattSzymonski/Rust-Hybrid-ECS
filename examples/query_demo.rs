/// Example showing the clean query2 API
use ecs_hybrid::*;

fn main() {
    println!("=== Clean Multi-Component Query Example ===\n");

    let scene = Scene::new();

    // Create some entities
    for i in 0..5 {
        let entity = scene.instantiate();
        entity
            .add_component(Name::new(format!("Entity_{}", i)))
            .add_component(Transform::new(i as f32, 0.0, 0.0))
            .add_component(Velocity::new(1.0, 0.5, 0.0));
    }

    scene.apply_commands();

    println!("Created 5 entities with Transform and Velocity\n");

    // Old way (verbose):
    println!("❌ OLD WAY (Verbose):");
    println!("─────────────────────");
    {
        let world_lock = scene.world();
        let world = world_lock.read();

        let entities: Vec<_> = world
            .query::<Transform>()
            .map(|iter| iter.map(|(e, _)| e).collect())
            .unwrap_or_default();

        for entity in entities {
            if let Some(velocity) = world.get_component::<Velocity>(entity) {
                if let Some(transform) = world.get_component::<Transform>(entity) {
                    println!(
                        "  Transform: ({:.1}, {:.1}), Velocity: ({:.1}, {:.1})",
                        transform.x, transform.y, velocity.x, velocity.y
                    );
                }
            }
        }
    }

    // New way (clean):
    println!("\n✅ NEW WAY (Clean):");
    println!("─────────────────────");
    {
        let world_lock = scene.world();
        let world = world_lock.read();

        // Just iterate over pairs!
        for (transform, velocity) in world.query2::<Transform, Velocity>() {
            println!(
                "  Transform: ({:.1}, {:.1}), Velocity: ({:.1}, {:.1})",
                transform.x, transform.y, velocity.x, velocity.y
            );
        }
    }

    // Mutable example:
    println!("\n✅ MUTABLE QUERY (Update transforms):");
    println!("─────────────────────────────────────");
    
    // Show before
    {
        let world_lock = scene.world();
        let world = world_lock.read();
        println!("Before update:");
        for (transform, velocity) in world.query2::<Transform, Velocity>() {
            println!(
                "  Transform: ({:.1}, {:.1}), Velocity: ({:.1}, {:.1})",
                transform.x, transform.y, velocity.x, velocity.y
            );
        }
    }
    
    // Perform update
    {
        let world_lock = scene.world();
        let mut world = world_lock.write();

        // Clean syntax for mutation!
        for (transform, velocity) in world.query2_mut::<Transform, Velocity>() {
            transform.x += velocity.x;
            transform.y += velocity.y;
        }
    }

    // Verify updates:
    {
        let world_lock = scene.world();
        let world = world_lock.read();

        println!("\nAfter update:");
        for (transform, velocity) in world.query2::<Transform, Velocity>() {
            println!(
                "  Transform: ({:.1}, {:.1}), Velocity: ({:.1}, {:.1})",
                transform.x, transform.y, velocity.x, velocity.y
            );
        }
    }

    println!("\n✓ Much cleaner and more ECS-like!");
}
