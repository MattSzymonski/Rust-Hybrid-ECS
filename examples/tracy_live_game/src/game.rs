//! The actual "game" for the `tracy_live` demo — components, systems, and
//! the initial entity population. **Try editing this while the host runs**
//! (e.g. tweak `gravity_system`'s formula or `health_decay_system`'s
//! increment) and save.

use ecs_hybrid::*;
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

#[derive(Debug, Clone)]
struct Mass(f32);
impl Component for Mass {}

#[derive(Debug, Clone)]
struct GravityForce {
    x: f32,
    y: f32,
}
impl Component for GravityForce {}

impl_trait_accessible!(dyn Component; Position, Velocity, Health, Enemy, Mass, GravityForce);

// ---- Systems ----

fn movement_system(mut query: Query<(&mut Position, &Velocity)>) {
    for (mut pos, vel) in query.iter_mut() {
        pos.x += vel.x;
        pos.y += vel.y;
    }
}

fn health_decay_system(mut query: Query<&mut Health>) {
    let _zone = ecs_hybrid::profile_scope!("health_decay_systXxxxxxm");

    // Parallel iteration
    let stats: query::ParForEachResult = query
        .par_iter_mut()
        .tracked()
        .label("health_decay_system")
        .for_each(|mut health| {
            health.0 = (health.0 + 0.1).max(0.0);
        });
    ecs_hybrid::profile_message!("movement: {}", stats);
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

/// Heavy per-entity work — trig, sqrt, mul — designed to stress
/// parallel distribution and make wake-up latency negligible.
/// Writes to its own `GravityForce` component so it can run in
/// parallel with `movement` and `health_decay`.
fn gravity_system(mut query: Query<(&mut GravityForce, &Mass)>) {
    let _zone = ecs_hybrid::profile_scope!("gravity_system");

    let stats = query
        .par_iter_mut()
        .tracked()
        .label("gravity_system")
        .for_each(|(mut force, mass)| {
            // Inverse-square gravity toward origin with cheap sqrt.
            let distance_sq = force.x * force.x + force.y.sqrt() * force.y + 0.01;
            let distance = distance_sq.sqrt();
            let magnitude = mass.0 / (distance_sq * distance); // 1/d³
            force.x = -force.x * magnitude.sqrt();
            force.y = -force.y * magnitude.sqrt();
            force.x = force.x.clamp(-1.0, 1.0);
            force.y = force.y.clamp(-1.0, 1.0);
        });
    ecs_hybrid::profile_message!("gravity: {}", stats);
}

#[allow(dead_code)]
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
            let _zone = ecs_hybrid::profile_scope!("lcg_init");
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

// ---- Setup ----

/// Registers components/systems and spawns the initial entity population.
/// Called by `game_setup` after [`Engine::reset_world`] on every (re)load.
pub fn setup(engine: &mut Engine) {
    engine.world_mut().register_component::<Position>();
    engine.world_mut().register_component::<Velocity>();
    engine.world_mut().register_component::<Health>();
    engine.world_mut().register_component::<Enemy>();
    engine.world_mut().register_component::<Mass>();
    engine.world_mut().register_component::<GravityForce>();

    engine.register_system("movement", movement_system);
    engine.register_system("health_decay", health_decay_system);
    // engine.register_system("collision_damage", collision_damage_system);
    // engine.register_system("enemy_ai", enemy_ai_system);
    engine.register_system("gravity", gravity_system);
    engine.register_system("cleanup", cleanup_system);
    //engine.register_system("spawner", spawner_system);

    engine.world_mut().reserve_entities(32000);
    for _ in 0..30000 {
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
            .with(Mass(1.0 + lcg() * 9.0))
            .with(GravityForce { x: 0.0, y: 0.0 })
            .with(Enemy)
            .build();
    }
}
