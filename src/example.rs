use crate::ecs_core::{Component, Entity, Position, ScriptComponent, UpdateContext, World};

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
