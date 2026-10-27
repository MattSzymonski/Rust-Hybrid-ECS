//! Entity creation and destruction benchmarks.
//!
//! Every spawned enemy, bullet or particle pays these costs, and they sit
//! outside the hot query loop, so this target isolates the allocation paths.
//!
//! # Responsibilities
//!
//! - Benchmarks entity creation with and without pre-allocation.
//! - Benchmarks entity destruction, covering entity-location removal,
//!   free-list push and empty-archetype cleanup.
//! - Benchmarks free-list ID recycling through a create-destroy-recreate cycle.
//! - Benchmarks wide-archetype spawning with six components per entity.
//!
//! # Design
//!
//! Each benchmark registers the components it uses on a fresh [`World`], so
//! every measurement starts from an empty state. The destroy and reuse-cycle
//! benchmarks use Criterion's `iter_batched` harness to keep setup outside the
//! measured section; the create benchmarks spawn inside the measurement,
//! because spawning is the workload.

#![allow(dead_code)]

// External crates
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

// Current crate
use pill_engine::*;

// =============================================================================
// Components
// =============================================================================

/// Position of an entity in 3D space.
///
/// Shared by the create, destroy, and reuse-cycle benchmarks as a
/// representative spatial component.
#[derive(Debug, Clone)]
struct Transform {
    x: f32,
    y: f32,
    z: f32,
}
impl Component for Transform {}

/// Linear velocity of an entity in 3D space.
///
/// Used alongside [`Transform`] so the benchmarks exercise multi-component
/// entity construction.
#[derive(Debug, Clone)]
struct Velocity {
    x: f32,
    y: f32,
    z: f32,
}
impl Component for Velocity {}

/// Flat hit-point value carried by spawned entities.
///
/// Tuple-style component that complements the struct-style [`Transform`] and
/// [`Velocity`] layouts.
#[derive(Debug, Clone)]
struct Health(f32);
impl Component for Health {}

// =============================================================================
// Benchmarks
// =============================================================================

/// Spawns `count` entities with three components each into the given `world`.
///
/// Shared helper for the create benches so every measurement exercises the
/// same entity-building path.
fn create_entities(world: &mut World, count: usize) {
    for _ in 0..count {
        world
            .create_entity()
            .with(Transform {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            })
            .with(Velocity {
                x: 0.1,
                y: 0.2,
                z: 0.3,
            })
            .with(Health(100.0))
            .build()
            .unwrap();
    }
}

/// Creates `count` entities with 3 components each in a fresh `World`.
/// Measures allocation overhead, HashMap insertion, and Vec growth during bulk spawning.
fn bench_create(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("entity_create");
    for &count in &[100, 1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                benchmark.iter(|| {
                    let mut world = World::new();
                    world.register_component::<Transform>();
                    world.register_component::<Velocity>();
                    world.register_component::<Health>();
                    create_entities(&mut world, count);
                    black_box(world.entity_count());
                });
            },
        );
    }
    group.finish();
}

/// Same as `bench_create` but calls `world.reserve_entities(count)` first.
/// Measures how much pre-allocation reduces reallocation overhead vs the non-reserved path.
fn bench_create_reserved(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("entity_create_reserved");
    for &count in &[1_000, 10_000, 100_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                benchmark.iter(|| {
                    let mut world = World::new();
                    world.register_component::<Transform>();
                    world.register_component::<Velocity>();
                    world.register_component::<Health>();
                    world.reserve_entities(count);
                    create_entities(&mut world, count);
                    black_box(world.entity_count());
                });
            },
        );
    }
    group.finish();
}

/// Destroys `count` single-component entities one by one.
/// Measures HashMap removal cost, free-list push overhead, and archetype cleanup.
fn bench_destroy(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("entity_destroy");
    for &count in &[100, 1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                benchmark.iter_batched(
                    || {
                        let mut world = World::new();
                        world.register_component::<Transform>();
                        let mut handles = Vec::with_capacity(count);
                        for i in 0..count {
                            handles.push(
                                world
                                    .create_entity()
                                    .with(Transform {
                                        x: i as f32,
                                        y: 0.0,
                                        z: 0.0,
                                    })
                                    .build()
                                    .unwrap(),
                            );
                        }
                        (world, handles)
                    },
                    |(mut world, handles)| {
                        for entity in handles {
                            black_box(world.destroy_entity(entity));
                        }
                    },
                    criterion::BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

/// Creates entities, destroys all of them, then immediately creates the same number again.
/// Exercises the free-list ID recycling path to measure reuse vs fresh-allocation overhead.
fn bench_reuse_cycle(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("entity_reuse_cycle");
    for &cycle_count in &[100, 1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(cycle_count),
            &cycle_count,
            |benchmark, &cycle_count| {
                benchmark.iter_batched(
                    || {
                        let mut world = World::new();
                        world.register_component::<Transform>();
                        world.register_component::<Velocity>();
                        world
                    },
                    |mut world| {
                        // Step 1: Create `cycle_count` entities, retaining their handles.
                        let mut handles = Vec::with_capacity(cycle_count);
                        for i in 0..cycle_count {
                            handles.push(
                                world
                                    .create_entity()
                                    .with(Transform {
                                        x: i as f32,
                                        y: 0.0,
                                        z: 0.0,
                                    })
                                    .with(Velocity {
                                        x: 0.1,
                                        y: 0.2,
                                        z: 0.3,
                                    })
                                    .build()
                                    .unwrap(),
                            );
                        }
                        // Step 2: Destroy every entity, pushing their freed IDs onto the free list.
                        for &entity in &handles {
                            let _ = world.destroy_entity(entity);
                        }
                        // Step 3: Re-create the same count, reusing freed IDs via the free list.
                        for i in 0..cycle_count {
                            black_box(
                                world
                                    .create_entity()
                                    .with(Transform {
                                        x: i as f32,
                                        y: 0.0,
                                        z: 0.0,
                                    })
                                    .with(Velocity {
                                        x: 0.1,
                                        y: 0.2,
                                        z: 0.3,
                                    })
                                    .build()
                                    .unwrap(),
                            );
                        }
                    },
                    criterion::BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

/// Creates entities with 6 components each, forming a single wide archetype.
/// Measures the cost of building large-archetype entities vs the 3-component baseline.
fn bench_create_many_components(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("entity_create_many_components");

    // Components used only by the wide-archetype benchmark.
    #[derive(Debug, Clone)]
    struct Armor(f32);
    impl Component for Armor {}
    #[derive(Debug, Clone)]
    struct Mana(f32);
    impl Component for Mana {}
    #[derive(Debug, Clone)]
    struct Stamina(f32);
    impl Component for Stamina {}

    for &count in &[100, 1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                benchmark.iter(|| {
                    let mut world = World::new();
                    world.register_component::<Transform>();
                    world.register_component::<Velocity>();
                    world.register_component::<Health>();
                    world.register_component::<Armor>();
                    world.register_component::<Mana>();
                    world.register_component::<Stamina>();
                    for i in 0..count {
                        world
                            .create_entity()
                            .with(Transform {
                                x: i as f32,
                                y: 0.0,
                                z: 0.0,
                            })
                            .with(Velocity {
                                x: 0.1,
                                y: 0.2,
                                z: 0.3,
                            })
                            .with(Health(100.0))
                            .with(Armor(50.0))
                            .with(Mana(200.0))
                            .with(Stamina(150.0))
                            .build()
                            .unwrap();
                    }
                    black_box(world.entity_count());
                });
            },
        );
    }
    group.finish();
}

// =============================================================================
// Criterion Entry Point
// =============================================================================

criterion_group!(
    benches,
    bench_create,
    bench_create_reserved,
    bench_destroy,
    bench_reuse_cycle,
    bench_create_many_components
);
criterion_main!(benches);
