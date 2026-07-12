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
use std::time::Instant;
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

/// Fast LCG random f32 — seeded from CPU counter, no syscalls.
fn lcg() -> f32 {
    #[cfg(target_arch = "x86_64")]
    fn seed() -> u64 {
        // RDTSC — fast, non-crypto seed. No syscall, no blocking.
        unsafe { std::arch::x86_64::_rdtsc() }
    }
    #[cfg(not(target_arch = "x86_64"))]
    fn seed() -> u64 {
        // Fallback: wall clock microseconds
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64
    }

    use std::cell::Cell;
    thread_local! {
        static S: Cell<u64> = {
            let _zone = crate::profile_scope!("lcg_init");
            Cell::new(seed().wrapping_mul(6364136223846793005).wrapping_add(1))
        };
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

    // Set 30 FPS cap using the engine's built-in limiter
    engine.set_fps_limit(200.0);
    engine.trace_frame_wait = false;
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
    println!("Target: 30 FPS (engine limiter)");
    println!();
    println!("Connect Tracy now. Press Ctrl+C to stop.");
    println!();

    let mut count: u64 = 0;
    let mut last_report = Instant::now();

    loop {
        engine.process_frame().unwrap();
        count += 1;

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
