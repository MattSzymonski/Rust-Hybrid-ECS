//! Archetype migration benchmarks for adding and removing components.
//!
//! Adding a component to an entity, or removing one, moves that entity to a
//! different archetype: every component it already has is copied into the
//! destination storage and the lookup tables are updated.
//!
//! # Responsibilities
//!
//! - Single add/remove: the baseline cost of a one-step archetype transition.
//! - Multi add/remove: three consecutive migrations per entity, stressing the
//!   move loop and column reallocation.
//! - Archetype explosion: four optional components combined per entity, so the
//!   world holds up to sixteen archetypes, stressing archetype lookup and
//!   storage fragmentation.
//!
//! # Design
//!
//! The add, remove and despawn benchmarks use `iter_batched`, so setup (entity
//! spawning, pre-adding components for the removal cases) is excluded from the
//! measured time. The archetype-explosion benchmark creates its entities inside
//! the measurement, because that is its workload. The fixture components
//! ([`Position`], [`Velocity`], [`Health`], [`Armor`], [`Mana`], [`Stamina`])
//! are cheap `Clone` structs, so the measured work is dominated by archetype
//! migration rather than component construction.

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
/// existing storage to clone and move.
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
    let mut handles = Vec::with_capacity(entity_count);
    for i in 0..entity_count {
        handles.push(
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

    // Step 3: Hand the populated world and its handles to the caller.
    (world, handles)
}

/// Measures the cost of adding a single `Health` component to entities that
/// already have `Position` + `Velocity`, triggering a one-step archetype migration.
fn bench_add_component(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("archetype_add_component");
    for &count in &[1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                benchmark.iter_batched(
                    || setup_world(count),
                    |(mut world, handles)| {
                        for entity in handles {
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

/// Measures the cost of removing a single `Health` component from entities,
/// including clone+move of remaining components into the destination archetype.
fn bench_remove_component(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("archetype_remove_component");
    for &count in &[1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                benchmark.iter_batched(
                    || {
                        // Setup phase: pre-add the component so the measured
                        // removal has something to migrate. Excluded from the
                        // timing window by `iter_batched`.
                        let (mut world, handles) = setup_world(count);
                        for &entity in &handles {
                            world
                                .add_component(entity, Health(entity.id() as f32))
                                .unwrap();
                        }
                        (world, handles)
                    },
                    |(mut world, handles)| {
                        for entity in handles {
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

/// Measures the cost of adding 3 components (`Health`, `Armor`, `Mana`) sequentially
/// to each entity, causing three consecutive archetype migrations per entity.
fn bench_add_multi_component(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("archetype_add_multi_component");
    for &count in &[1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                benchmark.iter_batched(
                    || setup_world(count),
                    |(mut world, handles)| {
                        for entity in handles {
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

/// Measures the cost of removing 3 components sequentially from each entity,
/// stressing the clone+move path across multiple archetype transitions.
fn bench_remove_multi_component(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("archetype_remove_multi_component");
    for &count in &[1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                benchmark.iter_batched(
                    || {
                        // Setup phase: pre-add the three components so the
                        // measured removals have something to migrate.
                        let (mut world, handles) = setup_world(count);
                        for &entity in &handles {
                            world.add_component(entity, Health(100.0)).unwrap();
                            world.add_component(entity, Armor(50.0)).unwrap();
                            world.add_component(entity, Mana(200.0)).unwrap();
                        }
                        (world, handles)
                    },
                    |(mut world, handles)| {
                        for entity in handles {
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

/// Creates entities carrying a different subset of four optional components,
/// filling the world with up to sixteen archetypes to stress archetype lookup
/// and storage overhead.
fn bench_archetype_explosion(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("archetype_explosion");
    // Four optional components give 2^4 = 16 archetypes at most, so the index
    // pattern decides which subset each entity lands in.
    for &count in &[100, 1_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                benchmark.iter(|| {
                    let mut world = World::new();
                    world.register_component::<Position>();
                    world.register_component::<Health>();
                    world.register_component::<Armor>();
                    world.register_component::<Mana>();
                    world.register_component::<Stamina>();

                    for i in 0..count {
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

// =============================================================================
// Criterion Entry Point
// =============================================================================

/// Measures LIFO despawn: destroying entities in the reverse of creation order.
///
/// This is the pattern projectile and particle systems produce - the most
/// recently spawned entity dies first - so every removal takes the last row and
/// the `index != last` guard in `ComponentColumn::swap_remove` and
/// `swap_remove_discard` skips the redundant self-copy. Pinning this path keeps
/// a regression in that guard visible.
fn bench_lifo_despawn(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("entity_lifo_despawn");
    for &count in &[1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                benchmark.iter_batched(
                    || setup_world(count),
                    |(mut world, handles)| {
                        // LIFO: destroy the most recently created entity first,
                        // which removes the last row of every column.
                        for entity in handles.into_iter().rev() {
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
