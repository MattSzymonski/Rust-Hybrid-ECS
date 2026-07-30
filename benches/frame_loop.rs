//! Frame Loop Benchmark
//! ====================
//!
//! This is the integration benchmark - it measures `engine.process_frame()`
//! wall-clock time, exercising every ECS subsystem together: system registration,
//! scheduler graph build + batch dispatch, query iteration (sequential and
//! parallel), change detection, and command execution.
//!
//! Six workload profiles span the spectrum from trivial to maximum stress:
//!
//! | Profile | Systems | Components/entity | Stress point |
//! |---------|---------|-------------------|--------------|
//! | `standard` | 3 light | ~40 B | Baseline frame overhead |
//! | `large_cache` | 4 (std + render) | ~300 B | 256 B component cache pressure |
//! | `light` | 3 tracy_live | ~40 B | Mixed seq/par with conditional logic |
//! | `heavy_compute` | 2 heavy | ~80 B | sqrt/div/cbrt per entity |
//! | `large_components` | 2 cache-heavy | ~420 B | 256 B + 128 B reads per entity |
//! | `full` | 7 tracy_live | ~460 B | All systems, all components, `With<Enemy>` filter |
//!
//! Every profile runs at multiple entity counts and includes a `_sequential`
//! baseline (parallel execution disabled) to measure the speedup from parallelism.
//!
//! Run with:
//! ```text
//! cargo bench --bench frame_loop
//! ```

#![allow(dead_code)]

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
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

/// 256 B - 4 cache lines per entity
#[derive(Debug, Clone)]
struct RenderData([[f64; 4]; 8]);
impl Component for RenderData {}

/// 128 B - 2 cache lines per entity
#[derive(Debug, Clone)]
struct PhysicsData([[f32; 4]; 8]);
impl Component for PhysicsData {}

impl_trait_accessible!(dyn Component; Position, Velocity, Health, Enemy, Mass, GravityForce, RenderData, PhysicsData);

// ---- Systems ----

fn movement_system(mut query: Query<(&mut Position, &Velocity)>) {
    for (mut position, velocity) in query.iter_mut() {
        position.x += velocity.x;
        position.y += velocity.y;
    }
}

fn health_decay_system(mut query: Query<&mut Health>) {
    query.par_iter_mut().for_each(|mut health| {
        health.0 = (health.0 + 0.1).max(0.0);
    });
}

fn collision_damage_system(mut query: Query<(&mut Health, &Position)>) {
    for (mut health, position) in query.iter_mut() {
        if position.x.abs() > 900.0 || position.y.abs() > 900.0 {
            health.0 -= 10.0;
        }
    }
}

fn enemy_ai_system(mut query: Query<(&mut Position, &mut Velocity), With<Enemy>>) {
    for (position, mut velocity) in query.iter_mut() {
        velocity.x = (position.y * 0.01).sin() * 0.5;
        velocity.y = (position.x * 0.01).cos() * 0.5;
    }
}

fn gravity_system(mut query: Query<(&mut GravityForce, &Mass)>) {
    query.par_iter_mut().for_each(|(mut force, mass)| {
        let distance_squared = force.x * force.x + force.y.sqrt() * force.y + 0.01;
        let distance = distance_squared.sqrt();
        let magnitude = mass.0 / (distance_squared * distance);
        force.x = (-force.x * magnitude.sqrt()).clamp(-1.0, 1.0);
        force.y = (-force.y * magnitude.sqrt()).clamp(-1.0, 1.0);
    });
}

fn cleanup_system(mut query: Query<(Entity, &mut Health)>) {
    query.par_iter_mut().for_each(|(entity, health)| {
        let mut accumulator = health.0 + entity.id() as f32;
        accumulator = (accumulator.sqrt() * accumulator.cbrt()).clamp(-100.0, 100.0);
        black_box(accumulator);
    });
}

fn render_system(mut query: Query<(&RenderData, &mut Health)>) {
    query.par_iter_mut().for_each(|(render_data, mut health)| {
        let accumulator =
            render_data.0[0][0] + render_data.0[2][1] + render_data.0[4][2] + render_data.0[6][3];
        health.0 = (health.0 + accumulator as f32 * 0.001).clamp(0.0, 200.0);
    });
}

fn physics_system(mut query: Query<(&PhysicsData, &mut Velocity)>) {
    query
        .par_iter_mut()
        .for_each(|(physics_data, mut velocity)| {
            let acceleration_x = physics_data.0[0][0] + physics_data.0[1][1];
            let acceleration_y = physics_data.0[2][2] + physics_data.0[3][3];
            velocity.x = (velocity.x + acceleration_x * 0.01).clamp(-5.0, 5.0);
            velocity.y = (velocity.y + acceleration_y * 0.01).clamp(-5.0, 5.0);
        });
}

fn reporting_system(mut query: Query<(&Position, &Health)>) {
    let mut count = 0usize;
    let mut total_health: f32 = 0.0;
    for (_, health) in query.iter_mut() {
        total_health += health.0;
        count += 1;
    }
    black_box((count, total_health));
}

// ---- Engine builder ----

#[derive(Clone, Copy, PartialEq, Eq)]
enum Profile {
    /// Original frame_loop: 3 light systems, small components
    Standard,
    /// Standard + 256 B RenderData cache pressure
    LargeCache,
    /// tracy_live light: movement, health_decay, collision
    Light,
    /// tracy_live heavy: gravity, cleanup
    HeavyCompute,
    /// tracy_live cache: render + physics (256 B + 128 B)
    LargeComponents,
    /// tracy_live full: all 7 systems with all components
    Full,
}

fn lcg(mut state: u64) -> (f32, u64) {
    state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    ((state >> 32) as f32 / u32::MAX as f32, state)
}

fn build_engine(entity_count: usize, profile: Profile, parallel: bool) -> Engine {
    let mut engine = Engine::new();
    engine.set_parallel_execution(parallel);

    // Always register base components
    engine.world_mut().register_component::<Position>();
    engine.world_mut().register_component::<Velocity>();
    engine.world_mut().register_component::<Health>();

    let needs_large = matches!(
        profile,
        Profile::LargeCache | Profile::LargeComponents | Profile::Full
    );
    let needs_heavy = matches!(profile, Profile::HeavyCompute | Profile::Full);

    if needs_large {
        engine.world_mut().register_component::<RenderData>();
        engine.world_mut().register_component::<PhysicsData>();
    }
    if needs_heavy {
        engine.world_mut().register_component::<Mass>();
        engine.world_mut().register_component::<GravityForce>();
    }
    if profile == Profile::Full {
        engine.world_mut().register_component::<Enemy>();
    }

    // Spawn entities
    let mut random_state: u64 = 0xDEAD_BEEF_CAFE_BABE;
    for i in 0..entity_count {
        let (position_x, next_state) = lcg(random_state);
        let (position_y, next_state) = lcg(next_state);
        let (velocity_x, next_state) = lcg(next_state);
        let (velocity_y, next_state) = lcg(next_state);
        random_state = next_state;

        let mut builder = engine
            .world_mut()
            .create_entity()
            .with(Position {
                x: (position_x - 0.5) * 1000.0,
                y: (position_y - 0.5) * 1000.0,
            })
            .with(Velocity {
                x: (velocity_x - 0.5) * 0.2,
                y: (velocity_y - 0.5) * 0.2,
            })
            .with(Health(100.0));

        if needs_heavy {
            let (mass_value, next_state) = lcg(random_state);
            random_state = next_state;
            builder = builder
                .with(Mass(1.0 + mass_value * 9.0))
                .with(GravityForce { x: 0.0, y: 0.0 });
        }
        if needs_large {
            builder = builder
                .with(RenderData([[0.0f64; 4]; 8]))
                .with(PhysicsData([[0.0f32; 4]; 8]));
        }
        if profile == Profile::Full && i % 4 == 0 {
            builder = builder.with(Enemy);
        }

        builder.build().unwrap();
    }

    // Register systems
    match profile {
        Profile::Standard => {
            engine.register_system("movement", movement_system);
            engine.register_system("health_decay", health_decay_system);
            engine.register_system("reporting", reporting_system);
        }
        Profile::LargeCache => {
            engine.register_system("movement", movement_system);
            engine.register_system("health_decay", health_decay_system);
            engine.register_system("reporting", reporting_system);
            engine.register_system("render", render_system);
        }
        Profile::Light => {
            engine.register_system("movement", movement_system);
            engine.register_system("health_decay", health_decay_system);
            engine.register_system("collision", collision_damage_system);
        }
        Profile::HeavyCompute => {
            engine.register_system("gravity", gravity_system);
            engine.register_system("cleanup", cleanup_system);
        }
        Profile::LargeComponents => {
            engine.register_system("render", render_system);
            engine.register_system("physics", physics_system);
        }
        Profile::Full => {
            engine.register_system("movement", movement_system);
            engine.register_system("health_decay", health_decay_system);
            engine.register_system("gravity", gravity_system);
            engine.register_system("render", render_system);
            engine.register_system("physics", physics_system);
            engine.register_system("cleanup", cleanup_system);
            engine.register_system("enemy_ai", enemy_ai_system);
            engine.register_system("collision", collision_damage_system);
        }
    }

    engine
}

/// Runs `engine.process_frame()` across 6 workload profiles (standard → full tracy_live),
/// each at multiple entity counts with both parallel and sequential execution.
/// Measures end-to-end frame cost - the ultimate integration metric for all ECS subsystems.
fn bench_frame_loop(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("frame_loop");

    let profiles: &[(&str, Profile, &[usize])] = &[
        ("standard", Profile::Standard, &[100_000, 500_000]),
        ("large_cache", Profile::LargeCache, &[100_000, 500_000]),
        ("light", Profile::Light, &[10_000, 100_000, 500_000]),
        (
            "heavy_compute",
            Profile::HeavyCompute,
            &[10_000, 50_000, 100_000],
        ),
        (
            "large_components",
            Profile::LargeComponents,
            &[10_000, 50_000],
        ),
        ("full", Profile::Full, &[10_000, 30_000]),
    ];

    for &(label, profile, entity_counts) in profiles {
        for &entity_count in entity_counts {
            // Parallel
            group.bench_with_input(
                BenchmarkId::new(label, entity_count),
                &entity_count,
                |benchmark, &entity_count| {
                    let mut engine = build_engine(entity_count, profile, true);
                    benchmark.iter(|| black_box(engine.process_frame().is_ok()));
                },
            );
            // Sequential baseline
            group.bench_with_input(
                BenchmarkId::new(&format!("{}_sequential", label), entity_count),
                &entity_count,
                |benchmark, &entity_count| {
                    let mut engine = build_engine(entity_count, profile, false);
                    benchmark.iter(|| black_box(engine.process_frame().is_ok()));
                },
            );
        }
    }

    group.finish();
}

criterion_group!(benches, bench_frame_loop);
criterion_main!(benches);
