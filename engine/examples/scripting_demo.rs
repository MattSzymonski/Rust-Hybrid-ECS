use pill_engine::*;
use trait_type_map::impl_trait_accessible;

/// A counter component that acts as a script
#[derive(Debug, Clone)]
struct Counter {
    value: i32,
    increment: i32,
    max_value: i32,
}

impl Component for Counter {}

impl ScriptComponent for Counter {
    fn update(&mut self, script_context: &mut ScriptContext) {
        // Modify self directly (always safe)
        self.value = (self.value + self.increment).min(self.max_value);
        println!("Counter: {} / {}", self.value, self.max_value);

        // Read component on own entity
        if let Some(position) =
            script_context.get_component::<Position>(script_context.get_owning_entity())
        {
            println!("  Position: ({}, {})", position.x, position.y);
        }

        // Mutate component on own entity (different type than self - safe)
        if let Some(position) =
            script_context.get_component_mut::<Position>(script_context.get_owning_entity())
        {
            position.y = self.value as f32;
            println!("  Updated Position.y to {}", position.y);
        }

        // Destroy entity when max reached (deferred - executes after all scripts)
        if self.value >= self.max_value {
            println!("  Counter reached max! Queueing destruction...");
            script_context.destroy_entity(script_context.get_owning_entity());
        }

        let entity: Entity = script_context.get_owning_entity().clone();

        script_context
            .get_commands()
            .add_component_to_entity(entity, Position { x: 42.0, y: 3.14 });
    }
}

/// A position component (not a script)
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct Position {
    x: f32,
    y: f32,
}

impl Component for Position {}

// Make components accessible through trait objects
impl_trait_accessible!(dyn Component; Counter, Position);

fn main() {
    println!("=== ECS Scripting Example ===\n");

    let mut engine = Engine::new();

    // Register components
    engine.world_mut().register_component::<Position>();
    engine.world_mut().register_script_component::<Counter>();

    // Create entity 1: Counter script only
    println!("Creating entity with counter...");
    let _entity2 = engine
        .world_mut()
        .create_entity()
        .with(Counter {
            value: 0,
            increment: 7,
            max_value: 50,
        })
        .build()
        .unwrap();

    // Create entity 2: Position + both scripts
    println!("Creating entity with position, and counter...\n");
    let _entity3 = engine
        .world_mut()
        .create_entity()
        .with(Position { x: 5.0, y: 10.0 })
        .with(Counter {
            value: 100,
            increment: 3,
            max_value: 120,
        })
        .build()
        .unwrap();

    // Simulate several frames
    for frame in 1..=8 {
        println!("--- Frame {} ---", frame);
        engine.process_frame().unwrap();
        println!();
    }

    println!("=== Scripting Example Complete ===");
}
