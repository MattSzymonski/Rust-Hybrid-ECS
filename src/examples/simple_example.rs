// ============================================================================
// Simple Example - Basic ECS Usage
// ============================================================================
//! This example demonstrates basic ECS functionality:
//! - Entity spawning with components
//! - Multiple system types with different parameter combinations
//! - Global components for singleton data
//! - Deferred command execution

use trait_type_map::impl_trait_accessible;

use crate::{Commands, Component, Engine, Entity, GlobalComponentQuery, Query, State, World};

// ============================================================================
// Example Components
// ============================================================================

#[derive(Debug)]
struct GlobalTime {
    delta_time: f32,
    elapsed_time: f32,
}

impl Component for GlobalTime {}

#[derive(Debug)]
struct Transform {
    x: f32,
    y: f32,
    z: f32,
}

impl Component for Transform {}

#[derive(Debug)]
struct Velocity {
    x: f32,
}

impl Component for Velocity {}

#[derive(Debug)]
struct Dead;

impl Component for Dead {}

impl_trait_accessible!(dyn Component; Dead, Velocity, Transform);

// ============================================================================
// Example Systems
// ============================================================================

fn movement_system(
    mut commands: Commands,
    mut query: Query<(Entity, &mut Transform, &Velocity)>,
    time_query: GlobalComponentQuery<GlobalTime>,
) {
    let delta_time = if let Some(global_time) = time_query.get() {
        global_time.delta_time
    } else {
        1.0
    };

    for (entity, transform, velocity) in query.iter_mut() {
        transform.x += velocity.x * delta_time;

        if transform.x > 100.0 {
            println!(
                "Entity {:?} exceeded x=100 (x={:.2}), queuing Dead tag",
                entity.id, transform.x
            );
            commands.add_component(entity, Dead);
        }
    }
}

fn dead_report_system(
    mut query: Query<(&Dead, &Transform)>,
    time_query: GlobalComponentQuery<GlobalTime>,
    State(last_report_time): State<&mut f32>,
) {
    let elapsed_time = if let Some(global_time) = time_query.get() {
        global_time.elapsed_time
    } else {
        0.0
    };

    if elapsed_time - *last_report_time >= 5.0 {
        println!("\n=== Dead Entities Report ===");
        for (_dead, transform) in query.iter_mut() {
            println!(
                "dead: transform=({:.2}, {:.2}, {:.2})",
                transform.x, transform.y, transform.z
            );
        }
        println!("===========================\n");
        *last_report_time = elapsed_time;
    }
}

fn time_print_system() {
    use std::time::SystemTime;

    if let Ok(duration) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        println!(
            "  [TimeSystem] Current Unix timestamp: {} seconds",
            duration.as_secs()
        );
    }
}

fn velocity_report_system(mut query: Query<(Entity, &Velocity)>) {
    println!("  [VelocityReport] Current velocities:");
    for (entity, velocity) in query.iter_mut() {
        println!("    Entity {:?}: velocity.x = {:.2}", entity.id, velocity.x);
    }
}

fn time_info_system(time: GlobalComponentQuery<GlobalTime>) {
    if let Some(global_time) = time.get() {
        println!(
            "  [TimeInfo] Delta: {:.2}s, Elapsed: {:.2}s",
            global_time.delta_time, global_time.elapsed_time
        );
    }
}

fn debug_system(
    mut query1: Query<&Transform>,
    mut query2: Query<&Velocity>,
    time: GlobalComponentQuery<GlobalTime>,
    State(report_time): State<&mut f32>,
) {
    if let Some(global_time) = time.get() {
        if global_time.elapsed_time - *report_time >= 3.0 {
            let transform_count = query1.iter_mut().count();
            let velocity_count = query2.iter_mut().count();
            println!(
                "  [Debug] Entities with Transform: {}, with Velocity: {}",
                transform_count, velocity_count
            );
            *report_time = global_time.elapsed_time;
        }
    }
}

// ============================================================================
// Main
// ============================================================================

pub fn main() {
    println!("=== Simple Example: Basic ECS Usage ===\n");

    let mut engine = Engine::new();
    let mut world = World::new();

    world.add_global_component(GlobalTime {
        delta_time: 1.0,
        elapsed_time: 0.0,
    });

    engine.register_system("movement_system", movement_system);
    engine.register_system("dead_report_system", dead_report_system);
    engine.register_system("time_print_system", time_print_system);
    engine.register_system("velocity_report_system", velocity_report_system);
    engine.register_system("time_info_system", time_info_system);
    engine.register_system("debug_system", debug_system);

    println!("Spawning entities...\n");

    let entity1 = world
        .spawn()
        .with(Transform {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        })
        .with(Velocity { x: 15.0 })
        .build();

    let entity2 = world
        .spawn()
        .with(Transform {
            x: 50.0,
            y: 5.0,
            z: 0.0,
        })
        .with(Velocity { x: 25.0 })
        .build();

    let entity3 = world
        .spawn()
        .with(Transform {
            x: 90.0,
            y: 0.0,
            z: 5.0,
        })
        .with(Velocity { x: 5.0 })
        .build();

    let entity4 = world
        .spawn()
        .with(Transform {
            x: 150.0,
            y: 10.0,
            z: 0.0,
        })
        .with(Dead)
        .build();

    println!(
        "Created entities: {:?}, {:?}, {:?}, {:?} (already dead)\n",
        entity1.id, entity2.id, entity3.id, entity4.id
    );

    for frame in 0..5 {
        if let Some(global_time) = world.get_global_component_mut::<GlobalTime>() {
            global_time.elapsed_time += global_time.delta_time;
        }

        println!("--- Frame {} ---", frame);
        engine.process_frame(&mut world);
        println!();
    }

    println!("=== Simple Example Complete ===");
}
