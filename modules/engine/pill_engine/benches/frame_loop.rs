//! Integration benchmark for the ECS frame loop.
//!
//! Measures `engine.process_frame()` wall-clock time, so one measurement
//! covers a complete frame. That frame includes the scheduler dispatch
//! (parallel batches or the sequential fallback), query iteration, change
//! tracking and the deferred command flush. The system set never changes here,
//! so the execution graph is built once per engine and only checked on later
//! frames.
//!
//! # Responsibilities
//!
//! - Benchmarks the frame loop across six workload profiles, from frame
//!   overhead alone to every system and component, at several entity counts.
//! - Pairs every run with a `_sequential` baseline (parallel execution
//!   disabled), so the speedup from parallelism is the ratio of the two series.
//!
//! # Design
//!
//! | Profile | Systems | Components | Stress point |
//! |---------|---------|------------|--------------|
//! | `standard` | movement, health_decay, reporting | Position, Velocity, Health | Frame overhead |
//! | `large_cache` | movement, health_decay, reporting, render | Position, Velocity, Health, RenderData (256 B), PhysicsData (128 B) | Cache-line pressure |
//! | `light` | movement, health_decay, collision | Position, Velocity, Health | Sequential and parallel systems, one with a branch |
//! | `heavy_compute` | gravity, cleanup | Position, Velocity, Health, Mass, GravityForce | sqrt/div/cbrt per entity |
//! | `large_components` | render, physics | Position, Velocity, Health, RenderData, PhysicsData | 256 B + 128 B rows per entity |
//! | `full` | all eight, `enemy_ai` included | every component, `Enemy` on every fourth entity | Everything at once |
//!
//! Run with:
//! ```text
//! cargo bench --bench frame_loop
//! ```

#![allow(dead_code)]

// External crates
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

// Current crate
use pill_engine::*;

// =============================================================================
// Components
// =============================================================================

/// Two-dimensional position of an entity in world space.
#[derive(Debug, Clone)]
struct Position {
    x: f32,
    y: f32,
}
impl Component for Position {}

/// Two-dimensional velocity of an entity, applied to position each frame.
#[derive(Debug, Clone)]
struct Velocity {
    x: f32,
    y: f32,
}
impl Component for Velocity {}

/// Current hit points of an entity, changed by the health and collision
/// systems.
#[derive(Debug, Clone)]
struct Health(f32);
impl Component for Health {}

/// Marker component that the enemy AI system filters on; the full profile tags
/// every fourth entity with it.
#[derive(Debug, Clone)]
struct Enemy;
impl Component for Enemy {}

/// Mass of an entity, used to scale the gravity force.
#[derive(Debug, Clone)]
struct Mass(f32);
impl Component for Mass {}

/// Gravity force acting on an entity, rewritten by the gravity system each
/// frame.
#[derive(Debug, Clone)]
struct GravityForce {
    x: f32,
    y: f32,
}
impl Component for GravityForce {}

/// A 256 B component, four cache lines per entity.
#[derive(Debug, Clone)]
struct RenderData([[f64; 4]; 8]);
impl Component for RenderData {}

/// A 128 B component, two cache lines per entity.
#[derive(Debug, Clone)]
struct PhysicsData([[f32; 4]; 8]);
impl Component for PhysicsData {}

// =============================================================================
// Profile
// =============================================================================

/// Workload profile selecting the systems and components one benchmark run
/// exercises.
///
/// Each variant has one row in the file header table. Together the variants
/// range from frame overhead alone to the heaviest component load.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Profile {
    /// Movement, health decay and reporting over small components.
    Standard,
    /// `Standard` plus `RenderData` and `PhysicsData`; only `RenderData` is
    /// read.
    LargeCache,
    /// Movement, health decay and collision damage on small components.
    Light,
    /// Gravity and cleanup, both compute-heavy.
    HeavyCompute,
    /// The render and physics systems over 256 B and 128 B components.
    LargeComponents,
    /// Every system and component, with a `With<Enemy>` filter.
    Full,
}

// =============================================================================
// Systems
// =============================================================================

/// Applies velocity to position for every entity each frame.
///
/// It runs on the sequential iterator, and most profiles include it.
fn movement_system(mut query: Query<(&mut Position, &Velocity)>) {
    for (mut position, velocity) in query.iter_mut() {
        position.x += velocity.x;
        position.y += velocity.y;
    }
}

/// Adds 0.1 to every entity's health each frame and never lets it go below
/// zero.
///
/// Runs on the parallel iterator over a single component.
fn health_decay_system(mut query: Query<&mut Health>) {
    query.par_iter_mut().for_each(|mut health| {
        health.0 = (health.0 + 0.1).max(0.0);
    });
}

/// Reduces health for entities that drift outside the central play area.
///
/// Runs on the sequential iterator and branches once per entity, so the light
/// profile includes uneven work.
fn collision_damage_system(mut query: Query<(&mut Health, &Position)>) {
    for (mut health, position) in query.iter_mut() {
        if position.x.abs() > 900.0 || position.y.abs() > 900.0 {
            health.0 -= 10.0;
        }
    }
}

/// Steers enemy entities with a sinusoidal rule over their position.
///
/// It is the only system that filters on `Enemy`, and only the `full` profile
/// registers it.
fn enemy_ai_system(mut query: Query<(&mut Position, &mut Velocity), With<Enemy>>) {
    for (position, mut velocity) in query.iter_mut() {
        velocity.x = (position.y * 0.01).sin() * 0.5;
        velocity.y = (position.x * 0.01).cos() * 0.5;
    }
}

/// Rewrites the gravity force from its own current x and y, scaled by mass,
/// with expensive math.
///
/// Heavy-compute workload: sqrt and division per entity on the parallel
/// iterator.
fn gravity_system(mut query: Query<(&mut GravityForce, &Mass)>) {
    query.par_iter_mut().for_each(|(mut force, mass)| {
        let distance_squared = force.x * force.x + force.y.sqrt() * force.y + 0.01;
        let distance = distance_squared.sqrt();
        let magnitude = mass.0 / (distance_squared * distance);
        force.x = (-force.x * magnitude.sqrt()).clamp(-1.0, 1.0);
        force.y = (-force.y * magnitude.sqrt()).clamp(-1.0, 1.0);
    });
}

/// Applies sqrt and cbrt math to every entity's health, then passes the result
/// through `black_box` so the compiler cannot remove the work.
///
/// Part of the heavy-compute workload.
fn cleanup_system(mut query: Query<(Entity, &mut Health)>) {
    query.par_iter_mut().for_each(|(entity, health)| {
        let mut accumulator = health.0 + entity.id() as f32;
        accumulator = (accumulator.sqrt() * accumulator.cbrt()).clamp(-100.0, 100.0);
        black_box(accumulator);
    });
}

/// Adds a small share of four `RenderData` values to health each frame. The
/// four values sit in four different cache lines, so an entity touches all
/// 256 B of its row.
///
/// Exercises 256 B component cache pressure in the `large_cache` and `full`
/// profiles.
fn render_system(mut query: Query<(&RenderData, &mut Health)>) {
    query.par_iter_mut().for_each(|(render_data, mut health)| {
        let accumulator =
            render_data.0[0][0] + render_data.0[2][1] + render_data.0[4][2] + render_data.0[6][3];
        health.0 = (health.0 + accumulator as f32 * 0.001).clamp(0.0, 200.0);
    });
}

/// Adds an acceleration built from four `PhysicsData` values to velocity each
/// frame. All four sit in the first 64 bytes of the row.
///
/// Exercises 128 B component cache pressure in the `large_components` and
/// `full` profiles: the scan moves 128 B per entity even though only the first
/// half of the row is read.
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

/// Counts entities and sums their health in one pass, then passes both totals
/// through `black_box` so the compiler keeps the loop.
///
/// Measures the cost of a sequential aggregation query over two components;
/// the position half of the query is never read.
fn reporting_system(mut query: Query<(&Position, &Health)>) {
    let mut entity_count = 0usize;
    let mut total_health: f32 = 0.0;
    for (_, health) in query.iter_mut() {
        total_health += health.0;
        entity_count += 1;
    }
    black_box((entity_count, total_health));
}

// =============================================================================
// Free Functions
// =============================================================================

/// Returns a pseudo-random value in `[0, 1]` and the next generator state.
///
/// This is a linear congruential generator. Each state is the previous state
/// times a constant plus another constant, wrapping at 64 bits. The value is
/// the state's high 32 bits divided by `u32::MAX`. Callers fill entity
/// components with the values, so every run does the same work.
fn next_pseudo_random_value(mut state: u64) -> (f32, u64) {
    state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    ((state >> 32) as f32 / u32::MAX as f32, state)
}

/// Builds an engine with `entity_count` entities and the system and component
/// set selected by `profile`.
///
/// It registers the components every profile needs, then the profile's own,
/// and spawns pseudo-random entities. The last step registers the profile's
/// systems. `parallel` turns parallel execution on or off; the sequential
/// baseline passes `false`.
fn build_engine(entity_count: usize, profile: Profile, parallel: bool) -> Engine {
    // Step 1: Create the engine and register the base components shared by every profile.
    let mut engine = Engine::new();
    engine.set_parallel_execution(parallel);

    engine.world_mut().register_component::<Position>();
    engine.world_mut().register_component::<Velocity>();
    engine.world_mut().register_component::<Health>();

    // Step 2: Register profile-specific components (large, heavy, or full sets).
    let uses_large_components = matches!(
        profile,
        Profile::LargeCache | Profile::LargeComponents | Profile::Full
    );
    let uses_heavy_components = matches!(profile, Profile::HeavyCompute | Profile::Full);

    if uses_large_components {
        engine.world_mut().register_component::<RenderData>();
        engine.world_mut().register_component::<PhysicsData>();
    }
    if uses_heavy_components {
        engine.world_mut().register_component::<Mass>();
        engine.world_mut().register_component::<GravityForce>();
    }
    if profile == Profile::Full {
        engine.world_mut().register_component::<Enemy>();
    }

    // Step 3: Spawn entities with deterministic pseudo-random component values.
    let mut random_state: u64 = 0xDEAD_BEEF_CAFE_BABE;
    for i in 0..entity_count {
        let (position_x, next_state) = next_pseudo_random_value(random_state);
        let (position_y, next_state) = next_pseudo_random_value(next_state);
        let (velocity_x, next_state) = next_pseudo_random_value(next_state);
        let (velocity_y, next_state) = next_pseudo_random_value(next_state);
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

        if uses_heavy_components {
            let (mass_value, next_state) = next_pseudo_random_value(random_state);
            random_state = next_state;
            builder = builder
                .with(Mass(1.0 + mass_value * 9.0))
                .with(GravityForce { x: 0.0, y: 0.0 });
        }
        if uses_large_components {
            builder = builder
                .with(RenderData([[0.0f64; 4]; 8]))
                .with(PhysicsData([[0.0f32; 4]; 8]));
        }
        if profile == Profile::Full && i % 4 == 0 {
            builder = builder.with(Enemy);
        }

        // Every component added above is registered, so the build cannot fail.
        builder.build().unwrap();
    }

    // Step 4: Register the system set for the selected profile.
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

/// Runs `engine.process_frame()` for every profile at its entity counts, once
/// parallel and once sequential.
///
/// Each measurement builds its engine before `benchmark.iter`, so the measured
/// time covers frames only. This is the whole-engine number: the systems run,
/// queries iterate the world, and the frame's deferred commands are applied.
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

    for &(profile_label, profile, entity_counts) in profiles {
        for &entity_count in entity_counts {
            // Parallel run: the systems run in scheduler batches.
            group.bench_with_input(
                BenchmarkId::new(profile_label, entity_count),
                &entity_count,
                |benchmark, &entity_count| {
                    let mut engine = build_engine(entity_count, profile, true);
                    benchmark.iter(|| black_box(engine.process_frame().is_ok()));
                },
            );
            // Sequential baseline: the same frame with parallel execution off,
            // so the ratio of the two series is what parallelism buys.
            group.bench_with_input(
                BenchmarkId::new(format!("{}_sequential", profile_label), entity_count),
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

// =============================================================================
// Bench Harness
// =============================================================================

criterion_group!(benches, bench_frame_loop);
criterion_main!(benches);
