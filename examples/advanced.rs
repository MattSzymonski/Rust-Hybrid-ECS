/// Advanced example showing more complex game scenarios
/// Run with: cargo run --example advanced
use ecs_hybrid::*;

// More complex components
#[derive(Debug, Clone)]
struct Sprite {
    texture: String,
    width: f32,
    height: f32,
}

impl Sprite {
    fn new(texture: impl Into<String>, width: f32, height: f32) -> Self {
        Self {
            texture: texture.into(),
            width,
            height,
        }
    }
}

#[derive(Debug, Clone)]
struct Collider {
    radius: f32,
}

impl Collider {
    fn new(radius: f32) -> Self {
        Self { radius }
    }
}

#[derive(Debug, Clone)]
struct Enemy {
    ai_type: String,
    aggro_range: f32,
}

impl Enemy {
    fn new(ai_type: impl Into<String>, aggro_range: f32) -> Self {
        Self {
            ai_type: ai_type.into(),
            aggro_range,
        }
    }
}

// Collision detection system
struct CollisionSystem;

impl System for CollisionSystem {
    fn execute(&mut self, world: &mut World, _delta_time: f32) {
        // Collect all entities with Transform and Collider
        let entities: Vec<_> = world
            .query::<Transform>()
            .map(|iter| iter.map(|(e, _)| e).collect())
            .unwrap_or_default();

        let mut collisions = Vec::new();

        // Simple collision detection
        for i in 0..entities.len() {
            for j in (i + 1)..entities.len() {
                let e1 = entities[i];
                let e2 = entities[j];

                if let (Some(t1), Some(c1)) = (
                    world.get_component::<Transform>(e1),
                    world.get_component::<Collider>(e1),
                ) {
                    if let (Some(t2), Some(c2)) = (
                        world.get_component::<Transform>(e2),
                        world.get_component::<Collider>(e2),
                    ) {
                        let dx = t1.x - t2.x;
                        let dy = t1.y - t2.y;
                        let distance = (dx * dx + dy * dy).sqrt();

                        if distance < c1.radius + c2.radius {
                            let n1 = world
                                .get_component::<Name>(e1)
                                .map(|n| n.value.clone())
                                .unwrap_or_else(|| format!("{:?}", e1));
                            let n2 = world
                                .get_component::<Name>(e2)
                                .map(|n| n.value.clone())
                                .unwrap_or_else(|| format!("{:?}", e2));
                            collisions.push((n1, n2));
                        }
                    }
                }
            }
        }

        // Print collisions
        for (name1, name2) in collisions {
            println!("  💥 Collision: {} <-> {}", name1, name2);
        }
    }
}

// AI system
struct AISystem;

impl System for AISystem {
    fn execute(&mut self, world: &mut World, _delta_time: f32) {
        // Find player position
        let player_pos = world.query::<Transform>().and_then(|mut iter| {
            iter.find_map(|(entity, transform)| {
                world
                    .get_component::<Name>(entity)
                    .filter(|n| n.value == "Player")
                    .map(|_| (transform.x, transform.y))
            })
        });

        if let Some((px, py)) = player_pos {
            // Update enemy velocities to chase player
            let enemies: Vec<Entity> = world
                .query::<Enemy>()
                .map(|iter| iter.map(|(e, _)| e).collect())
                .unwrap_or_default();

            for enemy in enemies {
                if let Some(transform) = world.get_component::<Transform>(enemy) {
                    let dx = px - transform.x;
                    let dy = py - transform.y;
                    let distance = (dx * dx + dy * dy).sqrt();

                    if let Some(enemy_comp) = world.get_component::<Enemy>(enemy) {
                        if distance < enemy_comp.aggro_range {
                            // Enemy in aggro range - chase player
                            let speed = 0.5;
                            let vx = (dx / distance) * speed;
                            let vy = (dy / distance) * speed;

                            if let Some(velocity) = world.get_component_mut::<Velocity>(enemy) {
                                velocity.x = vx;
                                velocity.y = vy;
                            }
                        }
                    }
                }
            }
        }
    }
}

// Render system (simulated)
struct RenderSystem;

impl System for RenderSystem {
    fn execute(&mut self, world: &mut World, _delta_time: f32) {
        let entities: Vec<Entity> = world
            .query::<Sprite>()
            .map(|iter| iter.map(|(e, _)| e).collect())
            .unwrap_or_default();

        println!("\n  🎨 Rendering {} sprites:", entities.len());

        for entity in entities {
            if let (Some(sprite), Some(transform)) = (
                world.get_component::<Sprite>(entity),
                world.get_component::<Transform>(entity),
            ) {
                let name = world
                    .get_component::<Name>(entity)
                    .map(|n| n.value.as_str())
                    .unwrap_or("Unknown");

                println!(
                    "     {} '{}' at ({:.1}, {:.1}) - {}x{}",
                    name, sprite.texture, transform.x, transform.y, sprite.width, sprite.height
                );
            }
        }
    }
}

fn main() {
    println!("=== Advanced Hybrid ECS Example ===\n");

    let scene = Scene::new();

    println!("--- Setting up game world ---");

    // Create player with full component set
    let player = scene.instantiate();
    player
        .add_component(Name::new("Player"))
        .add_component(Transform::new(0.0, 0.0, 0.0))
        .add_component(Velocity::new(0.0, 0.0, 0.0))
        .add_component(Health::new(100.0))
        .add_component(Sprite::new("player.png", 32.0, 32.0))
        .add_component(Collider::new(16.0));

    println!("✓ Player created");

    // Create multiple enemies
    for i in 0..3 {
        let angle = (i as f32) * 2.0 * std::f32::consts::PI / 3.0;
        let distance = 50.0;
        let x = angle.cos() * distance;
        let y = angle.sin() * distance;

        let enemy = scene.instantiate();
        enemy
            .add_component(Name::new(format!("Enemy_{}", i)))
            .add_component(Transform::new(x, y, 0.0))
            .add_component(Velocity::new(0.0, 0.0, 0.0))
            .add_component(Health::new(50.0))
            .add_component(Sprite::new("enemy.png", 32.0, 32.0))
            .add_component(Collider::new(16.0))
            .add_component(Enemy::new("chaser", 80.0));

        println!("✓ Enemy_{} created at ({:.1}, {:.1})", i, x, y);
    }

    // Create some obstacles
    let obstacle = scene.instantiate();
    obstacle
        .add_component(Name::new("Wall"))
        .add_component(Transform::new(25.0, 0.0, 0.0))
        .add_component(Sprite::new("wall.png", 64.0, 64.0))
        .add_component(Collider::new(32.0));

    println!("✓ Wall created");

    scene.apply_commands();

    // Setup systems
    let mut executor = SystemExecutor::new();
    executor.add_system(AISystem);
    executor.add_system(MovementSystem);
    executor.add_system(CollisionSystem);
    executor.add_system(RenderSystem);

    println!("\n--- Running game simulation ---");

    // Simulate 5 frames
    for frame in 1..=5 {
        println!("\n▶ Frame {}:", frame);

        // Simulate player movement (normally from input)
        {
            let world_lock = scene.world();
            let world = world_lock.read();
            let player_entity = world.query::<Name>().and_then(|mut iter| {
                iter.find_map(|(e, n)| if n.value == "Player" { Some(e) } else { None })
            });

            if let Some(player_entity) = player_entity {
                drop(world);
                let mut world = world_lock.write();

                if let Some(velocity) = world.get_component_mut::<Velocity>(player_entity) {
                    // Simulate player input
                    velocity.x = 2.0;
                    velocity.y = 1.0;
                }
            }
        }

        // Execute all systems
        {
            let world_lock = scene.world();
            let mut world = world_lock.write();
            executor.execute(&mut world, 0.016); // ~60 FPS
        }

        scene.apply_commands();

        // Small delay for readability
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    println!("\n--- Spawning projectiles dynamically ---");

    // Spawn projectiles (like in gameplay)
    for i in 0..3 {
        let projectile = scene.instantiate();
        projectile
            .add_component(Name::new(format!("Projectile_{}", i)))
            .add_component(Transform::new(0.0, 0.0, 0.0))
            .add_component(Velocity::new(10.0, 0.0, 0.0))
            .add_component(Sprite::new("bullet.png", 8.0, 8.0))
            .add_component(Collider::new(4.0));

        println!("✓ Spawned Projectile_{}", i);
    }

    scene.apply_commands();

    println!("\n--- Final world state ---");
    {
        let world_lock = scene.world();
        let world = world_lock.read();

        let entity_count = world.query::<Name>().map(|iter| iter.count()).unwrap_or(0);

        println!("Total entities: {}", entity_count);

        // Collect entity info to avoid lifetime issues
        let entities_info: Vec<(String, bool, bool, bool)> = world
            .query::<Name>()
            .map(|query| {
                query
                    .map(|(entity, name)| {
                        let has_velocity = world.get_component::<Velocity>(entity).is_some();
                        let has_collider = world.get_component::<Collider>(entity).is_some();
                        let has_ai = world.get_component::<Enemy>(entity).is_some();
                        (name.value.clone(), has_velocity, has_collider, has_ai)
                    })
                    .collect()
            })
            .unwrap_or_default();

        drop(world);
        drop(world_lock);

        println!("\nAll entities:");
        for (name, has_velocity, has_collider, has_ai) in entities_info {
            print!("  - {}", name);
            if has_velocity {
                print!(" [Moving]");
            }
            if has_collider {
                print!(" [Collidable]");
            }
            if has_ai {
                print!(" [AI]");
            }
            println!();
        }
    }

    println!("\n=== Summary ===");
    println!("✓ Created complex game world with multiple entity types");
    println!("✓ Ran multiple systems in sequence (AI, Movement, Collision, Render)");
    println!("✓ Dynamically spawned entities during gameplay");
    println!("✓ All with Unity-like API backed by ECS performance!");
    println!("\n✓ Advanced demo completed!");
}
