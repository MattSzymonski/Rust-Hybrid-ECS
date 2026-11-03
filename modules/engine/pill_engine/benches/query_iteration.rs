//! Query iteration benchmarks: one criterion group per way of walking a query,
//! each with its own cost profile.
//!
//! # Responsibilities
//!
//! - Owns the criterion harness for every query iteration variant.
//! - Covers the sequential, filtered, parallel, random-access and
//!   cache-pressure paths, so a regression in any query mode shows up in this
//!   target.
//!
//! # Design
//!
//! Each `bench_*` function builds its data before `benchmark.iter` and runs
//! only the measured work inside it, so setup is paid once per measurement,
//! not once per iteration. [`setup_world`] and [`setup_filtered_world`] supply
//! the shared worlds, and most benchmarks iterate a typed [`Query`] over them.
//! The filtered world tags every 4th entity [`Enemy`] and every 7th [`Frozen`],
//! so `With`/`Without`/`Or` see uneven match rates.
//!
//! - Sequential: unfiltered, `&mut`, three-component and `Entity`-only loops,
//!   plus the `Changed`/`Added` change-detection filters.
//! - Parallel: unfiltered, `With`-filtered, batch-size scaling from 1 to 1024,
//!   and a sequential-vs-parallel crossover sweep.
//! - Access: `get_component` random access against batched iteration, and the
//!   `entity_count`/`is_empty`/`first` helpers.
//! - Cache pressure: 64 B and 256 B components at high entity counts.
//! - `slice_ceiling`: the same per-row work over two plain slices, the
//!   cheapest version of it, which the query variants are measured against.

#![allow(dead_code)]

// External crates
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

// Current crate
use pill_engine::*;

// =============================================================================
// Components
// =============================================================================

/// Two-dimensional position component.
///
/// Most queries in this suite read or write it, so nearly every benchmark has
/// it in play.
#[derive(Debug, Clone)]
struct Position {
    x: f32,
    y: f32,
}
impl Component for Position {}

/// Two-dimensional velocity component.
///
/// Paired with [`Position`] in the two-component queries that form the
/// sequential and parallel baselines.
#[derive(Debug, Clone)]
struct Velocity {
    x: f32,
    y: f32,
}
impl Component for Velocity {}

/// Scalar hit-point component.
///
/// The third component in the multi-component query benchmark, and the
/// companion component in the cache-pressure benchmarks.
#[derive(Debug, Clone)]
struct Health(f32);
impl Component for Health {}

/// Tag component for the `With`/`Without` filter benchmarks.
///
/// Applied to every 4th entity, so filtered queries match about a quarter of
/// the entities.
#[derive(Debug, Clone)]
struct Enemy;
impl Component for Enemy {}

/// Tag component for the `Or` filter benchmarks.
///
/// Applied to every 7th entity, so the `Or<(With<Enemy>, With<Frozen>)>` query
/// has to check several filter pairs.
#[derive(Debug, Clone)]
struct Frozen;
impl Component for Frozen {}

/// Large component: a 4×4 matrix of `f32`, 64 bytes per row.
///
/// One row fills a whole cache line, so the cache-pressure benchmark shows what
/// that costs at high entity counts.
#[derive(Debug, Clone)]
struct LargeMatrix([[f32; 4]; 4]); // 4×4 matrix = 16 × 4B = 64 B
impl Component for LargeMatrix {}

/// Massive component: an 8×4 matrix of `f64`, 256 bytes per row.
///
/// Four cache lines per row, so the cache-pressure benchmark shows the memory
/// traffic and the cache misses at extreme entity counts.
#[derive(Debug, Clone)]
struct MassiveMatrix([[f64; 4]; 8]); // 8×4 f64 = 32 × 8B = 256 B
impl Component for MassiveMatrix {}

// =============================================================================
// Setup
// =============================================================================

/// Builds a [`World`] with `entity_count` entities, each carrying
/// [`Position`], [`Velocity`] and [`Health`].
///
/// This is the starting world for most of the benchmarks here: the unfiltered,
/// mutable, parallel, change-detection, helper and random-access ones. The
/// component values are fixed, so every iteration does the same work.
fn setup_world(entity_count: usize) -> World {
    // Step 1: Register the three components the queries read.
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();

    // Step 2: Spawn `entity_count` entities with deterministic component values.
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

/// Builds a [`World`] like [`setup_world`], tagging every 4th entity
/// [`Enemy`] and every 7th entity [`Frozen`].
///
/// Used by the `With`/`Without`/`Or` filter benchmarks: the tags spread the
/// entities over several archetypes, so a query has to skip some of them and
/// keep others.
fn setup_filtered_world(entity_count: usize) -> World {
    // Step 1: Register all five components, including the two tag types.
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();
    world.register_component::<Enemy>();
    world.register_component::<Frozen>();

    // Step 2: Spawn entities, tagging every 4th with `Enemy` and every 7th with `Frozen`.
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

// =============================================================================
// Benchmarks
// =============================================================================

/// Sequential iteration over `(&Position, &Velocity)` with no filter, the
/// simplest query path.
///
/// Measures the baseline cost per row: matching archetypes, finding the
/// component data, and adding the values up.
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

/// Sequential mutable iteration: writes `Position` while reading `Velocity`.
///
/// Each mutable access also stores the row's change tick, so this measures the
/// `&mut` cost on top of the unfiltered one.
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

/// Sequential iteration filtered by `Changed<Position>`, run after every
/// position has been written once.
///
/// Measures the per-entity tick comparison that the change-detection filter
/// adds.
fn bench_iter_changed(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("query_iter_changed");
    for &count in &[1_000, 10_000, 100_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                let mut world = setup_world(count);
                // Write every position once before measuring, so `Changed<Position>`
                // matches every row.
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

/// Read-only iteration through the parallel query iterator.
///
/// Measures what the parallel path adds to the row work: splitting the rows
/// into slices and handing them to the thread pool.
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

/// Parallel iteration with an explicit batch size, from 1 to 1024, over a
/// fixed 10K entities.
///
/// Shows which batch size balances dispatch overhead against cache locality
/// best.
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

/// Runs the same read-only query sequentially and in parallel, at entity
/// counts from 1K to 100K.
///
/// The two series show the count at which parallel iteration overtakes the
/// sequential one.
fn bench_crossover(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("query_crossover");
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

/// Benchmarks `entity_count()`, `is_empty()` and `first()` on a query over
/// 10K entities.
///
/// Measures the cost of the fast paths that answer from the query's own
/// bookkeeping, without visiting every row.
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
///
/// Larger components mean fewer entities per cache line and more memory
/// traffic, so this shows how the cache behaves under that pressure.
fn bench_large_component(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("query_large_component");

    for &count in &[10_000, 50_000] {
        group.bench_with_input(
            BenchmarkId::new("64B_component", count),
            &count,
            |benchmark, &count| {
                let mut world = World::new();
                world.register_component::<Health>();
                world.register_component::<LargeMatrix>();
                for i in 0..count {
                    world
                        .create_entity()
                        .with(Health((i % 100) as f32))
                        .with(LargeMatrix([[i as f32; 4]; 4]))
                        .build()
                        .unwrap();
                }
                benchmark.iter(|| {
                    let mut query = Query::<(&Health, &LargeMatrix)>::new(&mut world);
                    query.par_iter_mut().for_each(|(health, large_matrix)| {
                        black_box(health.0 + large_matrix.0[0][0]);
                    });
                });
            },
        );
    }

    for &count in &[500_000, 2_000_000] {
        group.bench_with_input(
            BenchmarkId::new("256B_component", count),
            &count,
            |benchmark, &count| {
                let mut world = World::new();
                world.register_component::<Health>();
                world.register_component::<MassiveMatrix>();
                for i in 0..count {
                    world
                        .create_entity()
                        .with(Health((i % 100) as f32))
                        .with(MassiveMatrix([[i as f64; 4]; 8]))
                        .build()
                        .unwrap();
                }
                benchmark.iter(|| {
                    let mut query = Query::<(&Health, &MassiveMatrix)>::new(&mut world);
                    query.par_iter_mut().for_each(|(health, massive_matrix)| {
                        black_box(health.0 + massive_matrix.0[0][0] as f32);
                    });
                });
            },
        );
    }

    group.finish();
}

/// Sequential iteration filtered by `With<Enemy>`, which matches about a
/// quarter of the entities.
///
/// Measures what skipping the archetypes without `Enemy` costs, compared with
/// iterating all entities.
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

/// Sequential iteration filtered by `Without<Frozen>`, which matches about
/// 86% of the entities.
///
/// Measures the exclusion-filter path, where most entities still pass the
/// check.
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

/// Sequential iteration with `Or<(With<Enemy>, With<Frozen>)>`, a filter that
/// holds several pairs.
///
/// Measures the cost of checking every pair, with OR logic, against each
/// archetype.
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

/// Sequential iteration filtered by `Added<Position>`, after the world moves
/// to its next tick.
///
/// Measures the per-row `Added` tick comparison. The entities were created at
/// tick 0 and the query's baseline stays at tick 0 too, so the filter compares
/// every row and rejects it: this times the comparison, not a match.
fn bench_iter_added(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("query_iter_added");
    for &count in &[1_000, 10_000, 100_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                let mut world = setup_world(count);
                // Move the world on to its next tick before measuring.
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

/// Iterates `Query<Entity>` with no component data, summing entity ids.
///
/// Measures the smallest possible per-entity cost: walking the archetypes and
/// reading nothing but the entity itself.
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

/// Random access through `world.get_component()` and `get_component_mut()` on
/// 10K entities.
///
/// Measures what one call costs - the component registry lookup, the entity
/// location lookup and the archetype lookup - compared with the batched
/// iteration path.
fn bench_get_component(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("query_get_component");
    let count = 10_000;

    group.bench_function("get_component_immutable", |benchmark| {
        let mut world = setup_world(count);
        // Collect every entity handle from a query, then drop the query.
        let entity_handles: Vec<Entity> = {
            let mut query = Query::<Entity>::new(&mut world);
            query.iter_mut().collect()
        };
        benchmark.iter(|| {
            let mut sum: f32 = 0.0;
            for &entity in &entity_handles {
                if let Some(position) = world.get_component::<Position>(entity) {
                    sum += position.x;
                }
            }
            black_box(sum);
        });
    });

    group.bench_function("get_component_mutable", |benchmark| {
        let mut world = setup_world(count);
        // Collect every entity handle from a query, then drop the query.
        let entity_handles: Vec<Entity> = {
            let mut query = Query::<Entity>::new(&mut world);
            query.iter_mut().collect()
        };
        benchmark.iter(|| {
            for &entity in &entity_handles {
                if let Some(position) = world.get_component_mut::<Position>(entity) {
                    position.x += 0.1;
                }
            }
        });
    });

    group.finish();
}

/// Parallel iteration with a `With<Enemy>` filter, reading only the ~25% of
/// entities that carry the tag.
///
/// Measures the combined cost of the parallel call and the filtered archetype
/// matching.
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

/// Sequential iteration over three components (`&Position`, `&Velocity`,
/// `&Health`) in one query.
///
/// Measures what each row costs when several component columns have to be
/// found, compared with a two-component query.
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

/// Bare-loop ceiling: the same per-row work the query benchmarks do, run over
/// two plain slices with no query machinery and no archetype join.
///
/// This is the cheapest way to do the work, so the query variants are measured
/// against it. The write half also stores a tick per row, the way change
/// tracking does in a mutable pass.
fn bench_slice_ceiling(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("slice_ceiling");
    for &count in &[10_000, 100_000] {
        // Step 1: two contiguous element columns, mirroring component storage.
        let positions: Vec<Position> = (0..count)
            .map(|i| Position {
                x: i as f32,
                y: (i * 2) as f32,
            })
            .collect();
        let velocities: Vec<Velocity> = (0..count).map(|_| Velocity { x: 0.1, y: 0.2 }).collect();

        // Step 2: read-only pass - two loads and an add per row.
        group.bench_with_input(BenchmarkId::new("read", count), &count, |benchmark, _| {
            benchmark.iter(|| {
                let mut sum: f32 = 0.0;
                for (position, velocity) in positions.iter().zip(velocities.iter()) {
                    sum += position.x + velocity.x;
                }
                black_box(sum);
            });
        });

        // Step 3: writable pass - the same work plus a per-row tick store.
        let mut mutable_positions = positions.clone();
        let mut ticks = vec![2u32; count];
        group.bench_with_input(BenchmarkId::new("write", count), &count, |benchmark, _| {
            benchmark.iter(|| {
                for ((position, velocity), tick) in mutable_positions
                    .iter_mut()
                    .zip(velocities.iter())
                    .zip(ticks.iter_mut())
                {
                    position.x += velocity.x;
                    position.y += velocity.y;
                    *tick = 9;
                }
                black_box(&mutable_positions[0].x);
            });
        });
    }
    group.finish();
}

// =============================================================================
// Criterion Entry Point
// =============================================================================

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
    bench_slice_ceiling,
);
criterion_main!(benches);
