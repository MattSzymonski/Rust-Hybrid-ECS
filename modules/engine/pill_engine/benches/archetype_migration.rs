//! Archetype migration benchmarks for adding and removing components.
//!
//! Adding a component to an entity, or removing one, moves the entity to a
//! different archetype. Every component the entity already has is copied into
//! the destination storage, and the lookup tables are updated.
//!
//! # Responsibilities
//!
//! - Single add/remove: the baseline cost of a one-step archetype transition.
//! - Multi add/remove: three migrations in a row per entity, which exercises
//!   moving an entity's rows and growing the destination columns.
//! - Archetype explosion: four optional components combined per entity, so the
//!   world holds up to sixteen archetypes, which exercises archetype lookup
//!   and spreads the storage over many small tables.
//!
//! # Design
//!
//! The add, remove and despawn benchmarks use `iter_batched`, so their setup
//! runs outside the measured time. Setup means spawning the entities and, for
//! the removal cases, adding the components that will be removed. The
//! archetype-explosion benchmark creates its entities inside the measurement,
//! because that is its workload.
//!
//! The fixture components ([`Position`], [`Velocity`], [`Health`], [`Armor`],
//! [`Mana`], [`Stamina`]) are flat `f32` structs, so building a component is
//! cheap. The measured work is the migration itself: moving every component
//! the entity already has into the destination storage.

#![allow(dead_code)]

// External crates
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

// Current crate
use pill_engine::*;

// =============================================================================
// Components
// =============================================================================

/// Two-dimensional position fixture carried by every spawned entity.
///
/// Serves as the baseline component so archetype migrations always have
/// existing storage to move.
#[derive(Debug, Clone)]
struct Position {
    x: f32,
    y: f32,
}
impl Component for Position {}

/// Two-dimensional velocity fixture paired with [`Position`] on every entity.
///
/// Gives spawned entities a second component so archetype migrations move
/// more than a single piece of storage.
#[derive(Debug, Clone)]
struct Velocity {
    x: f32,
    y: f32,
}
impl Component for Velocity {}

/// Flat health-points fixture.
///
/// Used as the component added and removed by the single-component benches.
#[derive(Debug, Clone)]
struct Health(f32);
impl Component for Health {}

/// Flat armor-points fixture.
///
/// Used alongside [`Health`] and [`Mana`] by the multi-component benches.
#[derive(Debug, Clone)]
struct Armor(f32);
impl Component for Armor {}

/// Flat mana-points fixture.
///
/// Used alongside [`Health`] and [`Armor`] by the multi-component benches.
#[derive(Debug, Clone)]
struct Mana(f32);
impl Component for Mana {}

/// Flat stamina-points fixture.
///
/// Used by the archetype-explosion bench to create distinct component
/// subsets across spawned entities.
#[derive(Debug, Clone)]
struct Stamina(f32);
impl Component for Stamina {}

// =============================================================================
// Benchmarks
// =============================================================================

/// Builds a fresh [`World`] with every fixture component registered and spawns
/// `entity_count` entities carrying [`Position`] + [`Velocity`].
///
/// Returns the world alongside the entity handles so benchmarks can migrate
/// each spawned entity during their measured phase.
fn setup_world(entity_count: usize) -> (World, Vec<Entity>) {
    // Step 1: Register every component type the benchmarks will touch.
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();
    world.register_component::<Armor>();
    world.register_component::<Mana>();
    world.register_component::<Stamina>();

    // Step 2: Spawn `entity_count` entities, each carrying Position + Velocity.
    let mut entity_handles = Vec::with_capacity(entity_count);
    for i in 0..entity_count {
        entity_handles.push(
            world
                .create_entity()
                .with(Position {
                    x: i as f32,
                    y: 0.0,
                })
                .with(Velocity { x: 0.1, y: 0.2 })
                .build()
                .unwrap(),
        );
    }

    // Step 3: Hand the populated world and its entity handles to the caller.
    (world, entity_handles)
}

/// Measures the cost of adding a single `Health` component to entities that
/// already have `Position` + `Velocity`. Each add is one archetype migration.
fn bench_add_component(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("archetype_add_component");
    for &entity_count in &[1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(entity_count),
            &entity_count,
            |benchmark, &entity_count| {
                benchmark.iter_batched(
                    || setup_world(entity_count),
                    |(mut world, entity_handles)| {
                        for entity in entity_handles {
                            black_box(
                                world
                                    .add_component(entity, Health(entity.id() as f32))
                                    .is_ok(),
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

/// Measures the cost of removing a single `Health` component from entities.
/// Each removal moves the components that remain into the destination
/// archetype.
fn bench_remove_component(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("archetype_remove_component");
    for &entity_count in &[1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(entity_count),
            &entity_count,
            |benchmark, &entity_count| {
                benchmark.iter_batched(
                    || {
                        // Setup phase: add the component first, so the measured
                        // removal has something to migrate. `iter_batched`
                        // keeps this out of the timing window.
                        let (mut world, entity_handles) = setup_world(entity_count);
                        for &entity in &entity_handles {
                            world
                                .add_component(entity, Health(entity.id() as f32))
                                .unwrap();
                        }
                        (world, entity_handles)
                    },
                    |(mut world, entity_handles)| {
                        for entity in entity_handles {
                            black_box(world.remove_component::<Health>(entity).is_ok());
                        }
                    },
                    criterion::BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

/// Measures the cost of adding three components (`Health`, `Armor`, `Mana`) to
/// each entity, one after another. Each add migrates the entity to a new
/// archetype.
fn bench_add_multi_component(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("archetype_add_multi_component");
    for &entity_count in &[1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(entity_count),
            &entity_count,
            |benchmark, &entity_count| {
                benchmark.iter_batched(
                    || setup_world(entity_count),
                    |(mut world, entity_handles)| {
                        for entity in entity_handles {
                            // Three adds: the entity migrates through three archetypes.
                            black_box(world.add_component(entity, Health(100.0)).is_ok());
                            black_box(world.add_component(entity, Armor(50.0)).is_ok());
                            black_box(world.add_component(entity, Mana(200.0)).is_ok());
                        }
                    },
                    criterion::BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

/// Measures the cost of removing three components from each entity, one after
/// another. Every removal moves the components that remain, so each entity
/// goes through several archetype transitions.
fn bench_remove_multi_component(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("archetype_remove_multi_component");
    for &entity_count in &[1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(entity_count),
            &entity_count,
            |benchmark, &entity_count| {
                benchmark.iter_batched(
                    || {
                        // Setup phase: add the three components first, so the
                        // measured removals have something to migrate.
                        let (mut world, entity_handles) = setup_world(entity_count);
                        for &entity in &entity_handles {
                            world.add_component(entity, Health(100.0)).unwrap();
                            world.add_component(entity, Armor(50.0)).unwrap();
                            world.add_component(entity, Mana(200.0)).unwrap();
                        }
                        (world, entity_handles)
                    },
                    |(mut world, entity_handles)| {
                        for entity in entity_handles {
                            black_box(world.remove_component::<Health>(entity).is_ok());
                            black_box(world.remove_component::<Armor>(entity).is_ok());
                            black_box(world.remove_component::<Mana>(entity).is_ok());
                        }
                    },
                    criterion::BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

/// Creates entities with different subsets of four optional components. The
/// world ends up with up to sixteen archetypes, which exercises archetype
/// lookup and storage overhead.
fn bench_archetype_explosion(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("archetype_explosion");
    // Four optional components allow at most 2^4 = 16 archetypes. Each
    // entity's index decides which components it gets, through the modulo
    // checks below.
    for &entity_count in &[100, 1_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(entity_count),
            &entity_count,
            |benchmark, &entity_count| {
                benchmark.iter(|| {
                    let mut world = World::new();
                    world.register_component::<Position>();
                    world.register_component::<Health>();
                    world.register_component::<Armor>();
                    world.register_component::<Mana>();
                    world.register_component::<Stamina>();

                    for i in 0..entity_count {
                        let mut builder = world.create_entity().with(Position {
                            x: i as f32,
                            y: 0.0,
                        });

                        if i % 2 == 0 {
                            builder = builder.with(Health(100.0));
                        }
                        if i % 3 == 0 {
                            builder = builder.with(Armor(50.0));
                        }
                        if i % 5 == 0 {
                            builder = builder.with(Mana(200.0));
                        }
                        if i % 7 == 0 {
                            builder = builder.with(Stamina(150.0));
                        }

                        builder.build().unwrap();
                    }
                    black_box(world.entity_count());
                });
            },
        );
    }
    group.finish();
}

/// Measures LIFO despawn: destroying entities in the reverse of creation order.
///
/// Projectile and particle systems tend to destroy the most recently spawned
/// entity first, which produces this order. Every removal then takes the last
/// row of each column, so the `index != last` guard in
/// `ComponentColumn::swap_remove` and `swap_remove_discard` skips the copy it
/// would otherwise make. This benchmark keeps a regression in that guard
/// visible.
fn bench_lifo_despawn(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("entity_lifo_despawn");
    for &entity_count in &[1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(entity_count),
            &entity_count,
            |benchmark, &entity_count| {
                benchmark.iter_batched(
                    || setup_world(entity_count),
                    |(mut world, entity_handles)| {
                        // LIFO: destroy the most recently created entity first,
                        // which removes the last row of every column.
                        for entity in entity_handles.into_iter().rev() {
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

// =============================================================================
// Criterion Entry Point
// =============================================================================

criterion_group!(
    benches,
    bench_add_component,
    bench_remove_component,
    bench_add_multi_component,
    bench_remove_multi_component,
    bench_archetype_explosion,
    bench_lifo_despawn,
);
criterion_main!(benches);
