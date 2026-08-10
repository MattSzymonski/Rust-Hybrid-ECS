//! Query Iteration Benchmarks
//! ==========================
//!
//! Queries are the hottest path in any ECS - every frame, every system spends
//! nearly all its time inside a query loop. These benchmarks cover every
//! meaningful iteration variant to build a complete performance profile.
//!
//! ## Categories
//!
//! Sequential - baseline per-row cost with no parallelism overhead.
//! Covers unfiltered, mutable (`&mut`), 3-component, and `Entity`-only queries.
//!
//! Filters - the added cost of skipping non-matching archetypes and entities.
//! Covers `With<T>`, `Without<T>`, `Or<(With<A>, With<B>)>`, `Changed<T>`, and
//! `Added<T>`. Each filter has a different archetype-matching cost profile.
//!
//! Parallel - Rayon-based distribution across threads. Covers unfiltered
//! parallel, filtered parallel (`With<T>`), batch-size scaling (1–1024),
//! and a sequential-vs-parallel crossover sweep to find the break-even point.
//!
//! Access patterns - `get_component()` random access (HashMap lookup per
//! entity) vs batched iteration, plus query helpers (`entity_count`, `is_empty`,
//! `first`).
//!
//! Cache pressure - 64 B and 256 B components at 10K–2M entities to measure
//! how component size affects cache-line utilization and memory bandwidth.

#![allow(dead_code)]

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
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
    x: f32,
    y: f32,
}
impl Component for Velocity {}

#[derive(Debug, Clone)]
struct Health(f32);
impl Component for Health {}

/// Tag component for With/Without filter benchmarks
#[derive(Debug, Clone)]
struct Enemy;
impl Component for Enemy {}

/// Tag component for Or filter benchmarks
#[derive(Debug, Clone)]
struct Frozen;
impl Component for Frozen {}

/// Large component - 64 bytes, spans 2 cache lines per entity.
#[derive(Debug, Clone)]
struct LargeData([[f32; 4]; 4]); // 4×4 matrix = 16 × 4B = 64 B
impl Component for LargeData {}

/// Massive component - 256 bytes, spans 4 cache lines per entity.
#[derive(Debug, Clone)]
struct MassiveData([[f64; 4]; 8]); // 8×4 f64 = 32 × 8B = 256 B
impl Component for MassiveData {}

impl_trait_accessible!(dyn Component; Position, Velocity, Health, Enemy, Frozen, LargeData, MassiveData);

fn setup_world(entity_count: usize) -> World {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();

    for i in 0..entity_count {
        world
            .create_entity()
            .with(Position {
                x: i as f32,
                y: (i * 2) as f32,
            })
            .with(Velocity { x: 0.1, y: 0.2 })
            .with(Health((i % 100) as f32))
            .build()
            .unwrap();
    }
    world
}

/// Setup with Enemy tag on every 4th entity, Frozen on every 7th.
fn setup_filtered_world(entity_count: usize) -> World {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();
    world.register_component::<Enemy>();
    world.register_component::<Frozen>();

    for i in 0..entity_count {
        let mut builder = world
            .create_entity()
            .with(Position {
                x: i as f32,
                y: (i * 2) as f32,
            })
            .with(Velocity { x: 0.1, y: 0.2 })
            .with(Health((i % 100) as f32));
        if i % 4 == 0 {
            builder = builder.with(Enemy);
        }
        if i % 7 == 0 {
            builder = builder.with(Frozen);
        }
        builder.build().unwrap();
    }
    world
}

/// Sequential iteration over `(&Position, &Velocity)` with no filter - the simplest query path.
/// Measures baseline per-row overhead including archetype matching, pointer resolution, and accumulation.
fn bench_iter_unfiltered(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("query_iter_unfiltered");
    for &count in &[1_000, 10_000, 100_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                let mut world = setup_world(count);
                benchmark.iter(|| {
                    let mut query = Query::<(&Position, &Velocity)>::new(&mut world);
                    let mut sum_x: f32 = 0.0;
                    for (position, velocity) in query.iter_mut() {
                        sum_x += position.x + velocity.x;
                    }
                    black_box(sum_x);
                });
            },
        );
    }
    group.finish();
}

/// Sequential mutable iteration writing `Position` while reading `Velocity`.
/// Adds change-detection tick writes on top of the unfiltered cost, measuring `&mut` overhead.
fn bench_iter_mutable(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("query_iter_mutable");
    for &count in &[1_000, 10_000, 100_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                let mut world = setup_world(count);
                benchmark.iter(|| {
                    let mut query = Query::<(&mut Position, &Velocity)>::new(&mut world);
                    for (mut position, velocity) in query.iter_mut() {
                        position.x += velocity.x;
                        position.y += velocity.y;
                    }
                });
            },
        );
    }
    group.finish();
}

/// Sequential iteration filtered by `Changed<Position>`, after mutating all positions first.
/// Measures the per-entity tick-comparison cost of the change-detection filter.
fn bench_iter_changed(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("query_iter_changed");
    for &count in &[1_000, 10_000, 100_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                let mut world = setup_world(count);
                // First frame: mutate all positions so Changed<Position> fires.
                {
                    let mut query = Query::<&mut Position>::new(&mut world);
                    for mut position in query.iter_mut() {
                        position.x += 1.0;
                    }
                }
                world.increment_change_tick();
                benchmark.iter(|| {
                    let mut query = Query::<(&Position,), Changed<Position>>::new(&mut world);
                    let mut matched_count = 0usize;
                    for (position,) in query.iter_mut() {
                        black_box(position.x);
                        matched_count += 1;
                    }
                    black_box(matched_count);
                });
            },
        );
    }
    group.finish();
}

/// Parallel read-only iteration distributing entities across Rayon threads.
/// Measures Rayon dispatch overhead + per-slice work-stealing cost vs the sequential baseline.
fn bench_par_iter_unfiltered(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("query_par_iter_unfiltered");
    for &count in &[10_000, 100_000, 1_000_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                let mut world = setup_world(count);
                benchmark.iter(|| {
                    let mut query = Query::<(&Position, &Velocity)>::new(&mut world);
                    query.par_iter_mut().for_each(|(position, velocity)| {
                        black_box(position.x + velocity.x);
                    });
                });
            },
        );
    }
    group.finish();
}

/// Parallel iteration with explicit batch sizes from 1 to 1024 at a fixed 10K entity count.
/// Reveals the optimal batch size that balances dispatch overhead against cache locality.
fn bench_par_batch_size_scaling(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("query_par_batch_size");
    let count = 10_000;

    for &batch_size in &[1, 16, 64, 256, 512, 1024] {
        group.bench_with_input(
            BenchmarkId::from_parameter(batch_size),
            &batch_size,
            |benchmark, &batch_size| {
                let mut world = setup_world(count);
                benchmark.iter(|| {
                    let mut query = Query::<(&Position, &Velocity)>::new(&mut world);
                    query.par_iter_mut().with_batch_size(batch_size).for_each(
                        |(position, velocity)| {
                            black_box(position.x + velocity.x);
                        },
                    );
                });
            },
        );
    }
    group.finish();
}

/// Runs the same read-only query both sequentially and in parallel at increasing entity counts.
/// Finds the crossover point where parallel iteration becomes faster than sequential.
fn bench_crossover(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("query_crossover");
    // Find the entity count where parallel beats sequential
    for &count in &[1_000, 5_000, 10_000, 25_000, 50_000, 100_000] {
        group.bench_with_input(
            BenchmarkId::new("sequential", count),
            &count,
            |benchmark, &count| {
                let mut world = setup_world(count);
                benchmark.iter(|| {
                    let mut query = Query::<(&Position, &Velocity)>::new(&mut world);
                    let mut sum = 0.0f32;
                    for (position, velocity) in query.iter_mut() {
                        sum += position.x + velocity.x;
                    }
                    black_box(sum);
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("parallel", count),
            &count,
            |benchmark, &count| {
                let mut world = setup_world(count);
                benchmark.iter(|| {
                    let mut query = Query::<(&Position, &Velocity)>::new(&mut world);
                    query.par_iter_mut().for_each(|(position, velocity)| {
                        black_box(position.x + velocity.x);
                    });
                });
            },
        );
    }
    group.finish();
}

/// Benchmarks `entity_count()`, `is_empty()`, and `first()` on a 10K-entity query.
/// Measures the cost of fast-path query metadata access without full iteration.
fn bench_query_helpers(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("query_helpers");
    let count = 10_000;

    group.bench_function("entity_count_unfiltered", |benchmark| {
        let mut world = setup_world(count);
        benchmark.iter(|| {
            let mut query = Query::<(&Position,)>::new(&mut world);
            black_box(query.entity_count());
        });
    });

    group.bench_function("is_empty_unfiltered", |benchmark| {
        let mut world = setup_world(count);
        benchmark.iter(|| {
            let mut query = Query::<(&Position,)>::new(&mut world);
            black_box(query.is_empty());
        });
    });

    group.bench_function("first_unfiltered", |benchmark| {
        let mut world = setup_world(count);
        benchmark.iter(|| {
            let mut query = Query::<(&Position,)>::new(&mut world);
            black_box(query.first());
        });
    });

    group.finish();
}

/// Parallel iteration over 64 B and 256 B components at high entity counts.
/// Stresses cache hierarchy - larger components mean fewer entities per cache line and more memory traffic.
fn bench_large_component(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("query_large_component");

    // 64 B component
    for &count in &[10_000, 50_000] {
        group.bench_with_input(
            BenchmarkId::new("64B_component", count),
            &count,
            |benchmark, &count| {
                let mut world = World::new();
                world.register_component::<Health>();
                world.register_component::<LargeData>();
                for i in 0..count {
                    world
                        .create_entity()
                        .with(Health((i % 100) as f32))
                        .with(LargeData([[i as f32; 4]; 4]))
                        .build()
                        .unwrap();
                }
                benchmark.iter(|| {
                    let mut query = Query::<(&Health, &LargeData)>::new(&mut world);
                    query.par_iter_mut().for_each(|(health, large_data)| {
                        black_box(health.0 + large_data.0[0][0]);
                    });
                });
            },
        );
    }

    // 256 B component - 4 cache lines per entity, extreme cache pressure
    for &count in &[500_000, 2_000_000] {
        group.bench_with_input(
            BenchmarkId::new("256B_component", count),
            &count,
            |benchmark, &count| {
                let mut world = World::new();
                world.register_component::<Health>();
                world.register_component::<MassiveData>();
                for i in 0..count {
                    world
                        .create_entity()
                        .with(Health((i % 100) as f32))
                        .with(MassiveData([[i as f64; 4]; 8]))
                        .build()
                        .unwrap();
                }
                benchmark.iter(|| {
                    let mut query = Query::<(&Health, &MassiveData)>::new(&mut world);
                    query.par_iter_mut().for_each(|(health, massive_data)| {
                        black_box(health.0 + massive_data.0[0][0] as f32);
                    });
                });
            },
        );
    }

    group.finish();
}

/// Sequential iteration filtered by `With<Enemy>`, matching ~25% of entities.
/// Measures the archetype-skipping cost of the `With` filter vs iterating all entities.
fn bench_iter_with_filter(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("query_iter_with");
    for &count in &[1_000, 10_000, 100_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                let mut world = setup_filtered_world(count);
                benchmark.iter(|| {
                    let mut query = Query::<(&Position,), With<Enemy>>::new(&mut world);
                    let mut sum: f32 = 0.0;
                    for (position,) in query.iter_mut() {
                        sum += position.x;
                    }
                    black_box(sum);
                });
            },
        );
    }
    group.finish();
}

/// Sequential iteration filtered by `Without<Frozen>`, matching ~86% of entities.
/// Measures the exclusion-filter path where most archetypes pass the check.
fn bench_iter_without_filter(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("query_iter_without");
    for &count in &[1_000, 10_000, 100_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                let mut world = setup_filtered_world(count);
                benchmark.iter(|| {
                    let mut query = Query::<(&Position,), Without<Frozen>>::new(&mut world);
                    let mut sum: f32 = 0.0;
                    for (position,) in query.iter_mut() {
                        sum += position.x;
                    }
                    black_box(sum);
                });
            },
        );
    }
    group.finish();
}

/// Sequential iteration with `Or<(With<Enemy>, With<Frozen>)>` - a multi-pair filter.
/// Measures the cost of evaluating multiple filter pairs (OR logic) per archetype.
fn bench_iter_or_filter(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("query_iter_or");
    for &count in &[1_000, 10_000, 100_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                let mut world = setup_filtered_world(count);
                benchmark.iter(|| {
                    let mut query =
                        Query::<(&Position,), Or<(With<Enemy>, With<Frozen>)>>::new(&mut world);
                    let mut sum: f32 = 0.0;
                    for (position,) in query.iter_mut() {
                        sum += position.x;
                    }
                    black_box(sum);
                });
            },
        );
    }
    group.finish();
}

/// Sequential iteration filtered by `Added<Position>` after a tick bump.
/// Measures the `Added` tick-comparison path where every entity matches on the first check.
fn bench_iter_added(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("query_iter_added");
    for &count in &[1_000, 10_000, 100_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                let mut world = setup_world(count);
                // Bump tick so Added<Position> fires for all entities
                world.increment_change_tick();
                benchmark.iter(|| {
                    let mut query = Query::<(&Position,), Added<Position>>::new(&mut world);
                    let mut matched_count = 0usize;
                    for (position,) in query.iter_mut() {
                        black_box(position.x);
                        matched_count += 1;
                    }
                    black_box(matched_count);
                });
            },
        );
    }
    group.finish();
}

/// Iterates `Query<Entity>` with no component data, summing entity IDs.
/// Measures the absolute minimum per-entity iteration cost - archetype walks with zero component access.
fn bench_entity_only_query(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("query_entity_only");
    for &count in &[10_000, 100_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                let mut world = setup_world(count);
                benchmark.iter(|| {
                    let mut query = Query::<Entity>::new(&mut world);
                    let mut identifier_sum: u64 = 0;
                    for entity in query.iter_mut() {
                        identifier_sum += entity.id();
                    }
                    black_box(identifier_sum);
                });
            },
        );
    }
    group.finish();
}

/// Random access via `world.get_component()` and `get_component_mut()` on 10K entities.
/// Measures HashMap lookup + component access cost vs the batched iteration path.
fn bench_get_component(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("query_get_component");
    let count = 10_000;

    group.bench_function("get_component_immutable", |benchmark| {
        let mut world = setup_world(count);
        // Collect all entity handles from a query, then drop the query.
        let handles: Vec<Entity> = {
            let mut query = Query::<Entity>::new(&mut world);
            query.iter_mut().collect()
        };
        benchmark.iter(|| {
            let mut sum: f32 = 0.0;
            for &entity in &handles {
                if let Some(position) = world.get_component::<Position>(entity) {
                    sum += position.x;
                }
            }
            black_box(sum);
        });
    });

    group.bench_function("get_component_mutable", |benchmark| {
        let mut world = setup_world(count);
        let handles: Vec<Entity> = {
            let mut query = Query::<Entity>::new(&mut world);
            query.iter_mut().collect()
        };
        benchmark.iter(|| {
            for &entity in &handles {
                if let Some(position) = world.get_component_mut::<Position>(entity) {
                    position.x += 0.1;
                }
            }
        });
    });

    group.finish();
}

/// Parallel iteration with `With<Enemy>` filter distributing the 25% entity subset across threads.
/// Measures the combined cost of parallel dispatch + filtered archetype matching.
fn bench_par_with_filter(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("query_par_with");
    for &count in &[10_000, 100_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                let mut world = setup_filtered_world(count);
                benchmark.iter(|| {
                    let mut query = Query::<(&Position,), With<Enemy>>::new(&mut world);
                    query.par_iter_mut().for_each(|(position,)| {
                        black_box(position.x);
                    });
                });
            },
        );
    }
    group.finish();
}

/// Sequential iteration over 3 components (`&Position, &Velocity, &Health`) in a single query.
/// Measures the per-row cost of resolving multiple component pointers vs a 2-component query.
fn bench_multi_component(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("query_multi_component");
    for &count in &[10_000, 100_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                let mut world = setup_world(count);
                benchmark.iter(|| {
                    let mut query = Query::<(&Position, &Velocity, &Health)>::new(&mut world);
                    let mut sum: f32 = 0.0;
                    for (position, velocity, health) in query.iter_mut() {
                        sum += position.x + velocity.x + health.0;
                    }
                    black_box(sum);
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_iter_unfiltered,
    bench_iter_mutable,
    bench_iter_changed,
    bench_par_iter_unfiltered,
    bench_par_batch_size_scaling,
    bench_crossover,
    bench_query_helpers,
    bench_large_component,
    bench_iter_with_filter,
    bench_iter_without_filter,
    bench_iter_or_filter,
    bench_iter_added,
    bench_entity_only_query,
    bench_get_component,
    bench_par_with_filter,
    bench_multi_component,
);
criterion_main!(benches);
