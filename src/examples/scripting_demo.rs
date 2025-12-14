use ecs_hybrid::*;
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
    fn update(&mut self, _entity: Entity, _world: &mut World) {
        self.value = (self.value + self.increment).min(self.max_value);
        println!("Counter: {} / {}", self.value, self.max_value);
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

pub fn main() {
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
        .build();

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
        .build();

    // Simulate several frames
    for frame in 1..=8 {
        println!("--- Frame {} ---", frame);
        engine.process_frame();
        println!();
    }

    println!("=== Scripting Example Complete ===");
}
