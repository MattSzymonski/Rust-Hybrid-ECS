//! Benchmarks for creating and destroying entities.
//!
//! A game pays these costs every time it spawns an enemy, a bullet or a
//! particle. They happen outside the hot query loop, so this target measures
//! the allocation work on its own.
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
//! Every benchmark registers its components on a fresh [`World`], so each
//! measurement starts from an empty world. The destroy and reuse-cycle
//! benchmarks use Criterion's `iter_batched` harness to keep their setup
//! outside the measured section. The create benchmarks build the world and
//! spawn inside the measurement, because spawning is what they measure.

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
/// Every benchmark uses it as the spatial component a real entity would
/// carry.
#[derive(Debug, Clone)]
struct Position {
    x: f32,
    y: f32,
    z: f32,
}
impl Component for Position {}

/// Linear velocity of an entity in 3D space.
///
/// Added next to [`Position`] so the spawning benchmarks build entities from
/// more than one component.
#[derive(Debug, Clone)]
struct Velocity {
    x: f32,
    y: f32,
    z: f32,
}
impl Component for Velocity {}

/// Hit points of a spawned entity, as a single flat number.
///
/// A tuple struct, unlike the struct-shaped [`Position`] and [`Velocity`], so
/// the benchmarks cover both component layouts.
#[derive(Debug, Clone)]
struct Health(f32);
impl Component for Health {}

// =============================================================================
// Benchmarks
// =============================================================================

/// Spawns `entity_count` entities with three components each into `world`.
///
/// `bench_create` and `bench_create_reserved` share this helper, so both
/// measure the same entity-building path.
///
/// # Panics
///
/// Panics if `world` has not registered `Position`, `Velocity` and `Health`,
/// because `build` then returns an error that the helper unwraps.
fn create_entities(world: &mut World, entity_count: usize) {
    for _ in 0..entity_count {
        world
            .create_entity()
            .with(Position {
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

/// Spawns a batch of entities with three components each into a new world,
/// and measures the cost of the spawn.
///
/// The measured section also builds the world and registers the three
/// components, so every result includes that fixed cost. What grows with the
/// batch is the insert into the map that records where each entity lives,
/// plus the archetype and its component columns.
fn bench_create(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("entity_create");
    for &entity_count in &[100, 1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(entity_count),
            &entity_count,
            |benchmark, &entity_count| {
                benchmark.iter(|| {
                    let mut world = World::new();
                    world.register_component::<Position>();
                    world.register_component::<Velocity>();
                    world.register_component::<Health>();
                    create_entities(&mut world, entity_count);
                    black_box(world.entity_count());
                });
            },
        );
    }
    group.finish();
}

/// Same as `bench_create`, but calls `world.reserve_entities(entity_count)`
/// before spawning the batch.
///
/// Compare its results with `bench_create` to see what pre-allocation saves.
/// `reserve_entities` reserves only the freed-id list and the entity location
/// map; the archetype and its component columns still grow during the spawn.
fn bench_create_reserved(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("entity_create_reserved");
    for &entity_count in &[1_000, 10_000, 100_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(entity_count),
            &entity_count,
            |benchmark, &entity_count| {
                benchmark.iter(|| {
                    let mut world = World::new();
                    world.register_component::<Position>();
                    world.register_component::<Velocity>();
                    world.register_component::<Health>();
                    world.reserve_entities(entity_count);
                    create_entities(&mut world, entity_count);
                    black_box(world.entity_count());
                });
            },
        );
    }
    group.finish();
}

/// Destroys a batch of single-component entities one at a time.
///
/// The setup closure spawns the batch outside the measured section. Inside it,
/// every destroy removes the entity from the location map and pushes its id
/// onto the freed-id list. The last one also removes the archetype, which is
/// empty by then.
fn bench_destroy(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("entity_destroy");
    for &entity_count in &[100, 1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(entity_count),
            &entity_count,
            |benchmark, &entity_count| {
                benchmark.iter_batched(
                    || {
                        let mut world = World::new();
                        world.register_component::<Position>();
                        let mut entity_handles = Vec::with_capacity(entity_count);
                        for i in 0..entity_count {
                            entity_handles.push(
                                world
                                    .create_entity()
                                    .with(Position {
                                        x: i as f32,
                                        y: 0.0,
                                        z: 0.0,
                                    })
                                    .build()
                                    .unwrap(),
                            );
                        }
                        (world, entity_handles)
                    },
                    |(mut world, entity_handles)| {
                        for entity in entity_handles {
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

/// Creates a batch of entities, destroys all of them, then creates the same
/// number again, all inside one measured iteration.
///
/// The second create takes back the ids the destroys returned, so comparing
/// this benchmark with `bench_create` shows what a recycled id costs against a
/// fresh one.
fn bench_reuse_cycle(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("entity_reuse_cycle");
    for &entity_count in &[100, 1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(entity_count),
            &entity_count,
            |benchmark, &entity_count| {
                benchmark.iter_batched(
                    || {
                        let mut world = World::new();
                        world.register_component::<Position>();
                        world.register_component::<Velocity>();
                        world
                    },
                    |mut world| {
                        // Step 1: Create `entity_count` entities and keep their
                        // entity handles.
                        let mut entity_handles = Vec::with_capacity(entity_count);
                        for i in 0..entity_count {
                            entity_handles.push(
                                world
                                    .create_entity()
                                    .with(Position {
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
                        // Step 2: Destroy every entity, which pushes each freed id
                        // onto the free list.
                        for &entity in &entity_handles {
                            let _ = world.destroy_entity(entity);
                        }
                        // Step 3: Create the same number again, taking the freed
                        // ids back off the free list.
                        for i in 0..entity_count {
                            black_box(
                                world
                                    .create_entity()
                                    .with(Position {
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

/// Spawns a batch of entities with six components each, so all of them share
/// one wide archetype.
///
/// Measures what building entities in a wide archetype costs. Compare the
/// result with `bench_create`, which spawns the same way with three
/// components, to see what the extra columns add.
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

    for &entity_count in &[100, 1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(entity_count),
            &entity_count,
            |benchmark, &entity_count| {
                benchmark.iter(|| {
                    let mut world = World::new();
                    world.register_component::<Position>();
                    world.register_component::<Velocity>();
                    world.register_component::<Health>();
                    world.register_component::<Armor>();
                    world.register_component::<Mana>();
                    world.register_component::<Stamina>();
                    for i in 0..entity_count {
                        world
                            .create_entity()
                            .with(Position {
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
