// Tracy Profiling Demo — runs continuously for live profiling.
//
// Usage:
//   1. Start Tracy GUI (Tracy.exe from https://github.com/wolfpld/tracy/releases)
//   2. cargo run --example tracy_live --release --features tracy
//   3. Click Connect in Tracy
//   4. Watch live CPU zones, frame times, and thread work distribution
//
// Press Ctrl+C to stop.

use ecs_hybrid::*;
use std::time::{Duration, Instant};
use trait_type_map::impl_trait_accessible;

// ---- Components ----

#[derive(Debug, Clone)]
struct Position {
    x: f32,
    y: f32,
}
impl Component for Position {}

#[derive(Debug, Clone)]
struct Velocity {
    x: f32,
    y: f32,
}
impl Component for Velocity {}

#[derive(Debug, Clone)]
struct Health(f32);
impl Component for Health {}

#[derive(Debug, Clone)]
struct Enemy;
impl Component for Enemy {}

impl_trait_accessible!(dyn Component; Position, Velocity, Health, Enemy);

// ---- Systems ----

fn movement_system(mut query: Query<(&mut Position, &Velocity)>) {
    for (mut pos, vel) in query.iter_mut() {
        pos.x += vel.x;
        pos.y += vel.y;
    }
}

fn health_decay_system(mut query: Query<&mut Health>) {
    for mut health in query.iter_mut() {
        health.0 = (health.0 - 0.1).max(0.0);
    }
}

fn collision_damage_system(mut query: Query<(&mut Health, &Position)>) {
    for (mut health, pos) in query.iter_mut() {
        if pos.x.abs() > 900.0 || pos.y.abs() > 900.0 {
            health.0 -= 10.0;
        }
    }
}

fn enemy_ai_system(mut query: Query<(&mut Position, &mut Velocity), With<Enemy>>) {
    for (pos, mut vel) in query.iter_mut() {
        vel.x = (pos.y * 0.01).sin() * 0.5;
        vel.y = (pos.x * 0.01).cos() * 0.5;
    }
}

fn cleanup_system(mut commands: Commands, mut query: Query<(Entity, &Health)>) {
    for (entity, health) in query.iter_mut() {
        if health.0 <= 0.0 {
            let _ = commands.destroy_entity(entity);
        }
    }
}

fn spawner_system(mut commands: Commands) {
    commands
        .create_entity()
        .with(Position {
            x: (lcg() - 0.5) * 1000.0,
            y: (lcg() - 0.5) * 1000.0,
        })
        .with(Velocity {
            x: (lcg() - 0.5) * 0.2,
            y: (lcg() - 0.5) * 0.2,
        })
        .with(Health(100.0))
        .with(Enemy)
        .build();
}

/// Fast LCG random f32 — simple, no deps.
fn lcg() -> f32 {
    use std::cell::Cell;
    use std::hash::{BuildHasher, RandomState};
    thread_local! {
        static S: Cell<u64> = Cell::new(RandomState::new().hash_one(0u64));
    }
    S.with(|s| {
        let mut x = s.get();
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        s.set(x);
        (x >> 32) as f32 / u32::MAX as f32
    })
}

// ---- Main ----

fn main() {
    crate::profile_init!();
    crate::profile_thread!("main");

    let mut engine = Engine::new();
    engine.set_parallel_execution(true);

    engine.world_mut().register_component::<Position>();
    engine.world_mut().register_component::<Velocity>();
    engine.world_mut().register_component::<Health>();
    engine.world_mut().register_component::<Enemy>();

    engine.register_system("movement", movement_system);
    engine.register_system("health_decay", health_decay_system);
    engine.register_system("collision_damage", collision_damage_system);
    engine.register_system("enemy_ai", enemy_ai_system);
    engine.register_system("cleanup", cleanup_system);
    engine.register_system("spawner", spawner_system);

    engine.world_mut().reserve_entities(2000);
    for _ in 0..1000 {
        let _ = engine
            .world_mut()
            .create_entity()
            .with(Position {
                x: (lcg() - 0.5) * 1000.0,
                y: (lcg() - 0.5) * 1000.0,
            })
            .with(Velocity {
                x: (lcg() - 0.5) * 0.2,
                y: (lcg() - 0.5) * 0.2,
            })
            .with(Health(100.0))
            .with(Enemy)
            .build();
    }

    println!("=== Tracy Live Profiling Demo ===");
    println!("6 systems, 1000 entities, parallel ON");
    println!("Target: 30 FPS (limiter ON)");
    println!();
    println!("Connect Tracy now. Press Ctrl+C to stop.");
    println!();

    const TARGET_FPS: f64 = 30.0;
    const FRAME_BUDGET: Duration = Duration::from_nanos((1_000_000_000.0 / TARGET_FPS) as u64);

    let mut count: u64 = 0;
    let mut last_report = Instant::now();

    loop {
        let frame_start = Instant::now();

        engine.process_frame().unwrap();
        crate::profile_frame_mark!();
        count += 1;

        // FPS limiter — sleep remaining frame budget
        let elapsed = frame_start.elapsed();
        if elapsed < FRAME_BUDGET {
            std::thread::sleep(FRAME_BUDGET - elapsed);
        }

        // Report every 2 seconds
        let dt = last_report.elapsed().as_secs_f64();
        if dt >= 2.0 {
            let fps = count as f64 / dt;
            let entities = engine.world().entity_count();
            println!("  {:>6.0} FPS | {:>5} entities", fps, entities);
            count = 0;
            last_report = Instant::now();
        }
    }
}
