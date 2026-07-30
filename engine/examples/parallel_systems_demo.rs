// Demo of parallel system execution

use ecs_hybrid::*;
use trait_type_map::impl_trait_accessible;

#[derive(Debug, Clone)]
struct Position {
    x: f32,
    y: f32,
}
impl Component for Position {}

#[derive(Debug, Clone)]
struct Velocity {
    vx: f32,
    vy: f32,
}
impl Component for Velocity {}

#[derive(Debug, Clone)]
struct Health(f32);
impl Component for Health {}

// Make components accessible through trait objects
impl_trait_accessible!(dyn Component; Position, Velocity, Health);

fn ttt_system(mut commands: Commands, mut query: Query<(&mut Position, &Velocity)>) {
    for (mut pos, vel) in query.iter_mut() {
        pos.x += vel.vx;
        pos.y += vel.vy;
    }

    let _x = commands
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .with(Velocity { vx: 1.0, vy: 1.0 })
        .with(Health(50.0))
        .build();
}

fn movement_system(mut query: Query<(&mut Position, &Velocity)>) {
    for (mut pos, vel) in query.iter_mut() {
        pos.x += vel.vx;
        pos.y += vel.vy;
    }
}

fn health_system(mut query: Query<&mut Health>) {
    for mut health in query.iter_mut() {
        health.0 = (health.0 - 0.1).max(0.0);
    }
}

fn damage_system(mut query: Query<(&mut Health, &Position)>) {
    for (mut health, pos) in query.iter_mut() {
        if pos.x.abs() < 1.0 && pos.y.abs() < 1.0 {
            health.0 = (health.0 - 1.0).max(0.0);
        }
    }
}

fn main() {
    println!("=== Parallel Systems Demo ===\n");

    let mut engine = Engine::new();
    engine.set_parallel_execution(true);

    // Register components
    engine.world_mut().register_component::<Position>();
    engine.world_mut().register_component::<Velocity>();
    engine.world_mut().register_component::<Health>();

    // Register systems - these can run in parallel!
    // movement_system: writes Position, reads Velocity
    // health_system: writes Health
    // No conflicts! Can run in parallel.
    engine.register_system("movement", movement_system);
    engine.register_system("health", health_system);
    engine.register_system("damage", damage_system);
    engine.register_system("xxx", ttt_system);

    // Print the execution graph
    println!("System Execution Graph:");
    engine.print_execution_graph();
    println!();

    // Create entities
    for i in 0..5 {
        engine
            .world_mut()
            .create_entity()
            .with(Position {
                x: i as f32,
                y: 0.0,
            })
            .with(Velocity { vx: 1.0, vy: 0.5 })
            .with(Health(100.0))
            .build()
            .unwrap();
    }

    println!("Running 5 frames...\n");
    for frame in 0..5 {
        println!("Frame {}", frame);
        engine.process_frame().unwrap();
    }

    println!("\nDemo complete!");
}
