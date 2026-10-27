//! Resource and commands benchmarks for the `pill_engine` world API.
//!
//! Resources are singleton data (time, input, config) stored in the `World`
//! and accessed through `Res<T>` / `ResMut<T>` system parameters. Commands are
//! the deferred-operation queue that lets a system schedule structural changes
//! - create or destroy entities, add components - without holding `&mut World`.
//!
//! # Responsibilities
//!
//! - Measures the four resource operations: insert (map insert plus heap
//!   allocation), get (immutable lookup and downcast), get_mut (mutable lookup
//!   and downcast, untracked) and remove (lookup removal and deallocation).
//! - Measures the deferred-command pipeline: queue the operation during system
//!   execution, apply it during the frame's command flush.
//! - Covers entity creation with two components, entity destruction, and
//!   component addition via `add_component_to_entity`.
//!
//! # Design
//!
//! Resource benchmarks drive a fresh [`World`] through the CRUD methods;
//! command benchmarks register a system that queues work and then run
//! `Engine::process_frame` to flush it. Fixture component and resource types
//! are defined here so every harness shares one set of registrations.

#![allow(dead_code)]

// External crates
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

// Current crate
use pill_engine::*;

// =============================================================================
// Components
// =============================================================================

/// Two-dimensional position of an entity in world space.
///
/// Bench fixture component used by the entity-spawning and archetype-migration
/// command benchmarks.
#[derive(Debug, Clone)]
struct Position {
    x: f32,
    y: f32,
}
impl Component for Position {}

/// Two-dimensional velocity of an entity in world space.
///
/// Bench fixture component paired with `Position` when spawning entities with
/// two components through the command queue.
#[derive(Debug, Clone)]
struct Velocity {
    x: f32,
    y: f32,
}
impl Component for Velocity {}

/// Flat health value carried by an entity.
///
/// Bench fixture component used to measure `add_component_to_entity` through
/// the deferred command queue.
#[derive(Debug, Clone)]
struct Health(f32);
impl Component for Health {}

// =============================================================================
// Resources
// =============================================================================

/// Accumulated project clock singleton resource.
///
/// Holds the per-frame delta and total elapsed time; the target of the
/// resource insert/get/get_mut/remove benchmarks.
#[derive(Debug)]
struct ProjectTime {
    delta: f32,
    elapsed: f32,
}
impl Resource for ProjectTime {}

/// Mouse position and button state singleton resource.
///
/// Declared for the fixture set; no benchmark registers or measures it.
#[derive(Debug)]
struct InputState {
    mouse_x: f32,
    mouse_y: f32,
    buttons: u32,
}
impl Resource for InputState {}

/// Global simulation configuration singleton resource.
///
/// Declared for the fixture set; no benchmark registers or measures it.
#[derive(Debug)]
struct Config {
    gravity: f32,
    max_entities: u32,
}
impl Resource for Config {}

// =============================================================================
// Benchmarks
// =============================================================================

/// Inserts the same `ProjectTime` resource `count` times into a fresh `World` (last write wins).
/// Measures the map insert plus the heap allocate-and-free each insert performs.
fn bench_resource_insert(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("resource_insert");
    for &count in &[100, 1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                benchmark.iter(|| {
                    let mut world = World::new();
                    for i in 0..count {
                        world.insert_resource(ProjectTime {
                            delta: 0.016,
                            elapsed: i as f32 * 0.016,
                        });
                    }
                    black_box(world.has_resource::<ProjectTime>());
                });
            },
        );
    }
    group.finish();
}

/// Reads a `ProjectTime` resource `count` times via `world.get_resource()` in a tight loop.
/// Measures the cost of the immutable HashMap lookup + downcast path for resources.
fn bench_resource_get(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("resource_get");
    for &count in &[1_000, 10_000, 100_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                let mut world = World::new();
                world.insert_resource(ProjectTime {
                    delta: 0.016,
                    elapsed: 0.0,
                });
                benchmark.iter(|| {
                    for _ in 0..count {
                        black_box(
                            world
                                .get_resource::<ProjectTime>()
                                .map(|resource| resource.elapsed),
                        );
                    }
                });
            },
        );
    }
    group.finish();
}

/// Mutates a `ProjectTime` resource `count` times via `world.get_resource_mut()`.
/// Measures the mutable lookup and downcast path; the untracked accessor leaves
/// change ticks alone.
fn bench_resource_get_mut(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("resource_get_mut");
    for &count in &[1_000, 10_000, 100_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                let mut world = World::new();
                world.insert_resource(ProjectTime {
                    delta: 0.016,
                    elapsed: 0.0,
                });
                benchmark.iter(|| {
                    for _ in 0..count {
                        if let Some(time) = world.get_resource_mut::<ProjectTime>() {
                            time.elapsed += time.delta;
                            black_box(time.elapsed);
                        }
                    }
                });
            },
        );
    }
    group.finish();
}

/// Removes and drops a `ProjectTime` resource from the `World`.
/// Measures the map removal and the deallocation of the erased value.
fn bench_resource_remove(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("resource_remove");
    for &count in &[100, 1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                benchmark.iter_batched(
                    || {
                        let mut world = World::new();
                        for i in 0..count {
                            world.insert_resource(ProjectTime {
                                delta: 0.016,
                                elapsed: i as f32,
                            });
                        }
                        world
                    },
                    |mut world| {
                        black_box(world.remove_resource::<ProjectTime>().ok());
                    },
                    criterion::BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

/// Queues `count` entity creations (2 components each) via `Commands`, then executes the queue.
/// Measures the full deferred-command pipeline: queue push + `process_frame` command application.
fn bench_commands_create_entity(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("commands_create_entity");
    for &count in &[100, 1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                // Engine construction is setup, not workload: `Engine::new`
                // warms the Rayon pool and prints the parallel-iteration
                // configuration, which would otherwise dominate a small
                // workload and hide the command pipeline the benchmark is for.
                benchmark.iter_batched(
                    || {
                        // Setup: a fresh engine with the component types the
                        // spawner writes. Excluded from the measurement.
                        let mut engine = Engine::new();
                        engine.world_mut().register_component::<Position>();
                        engine.world_mut().register_component::<Velocity>();
                        engine
                    },
                    |mut engine| {
                        // Measured: register the spawning system, queue `count`
                        // creations through `Commands`, then flush them in
                        // `process_frame`.
                        engine.register_system("spawner", move |mut commands: Commands| {
                            for i in 0..count {
                                commands
                                    .create_entity()
                                    .with(Position {
                                        x: i as f32,
                                        y: 0.0,
                                    })
                                    .with(Velocity { x: 0.1, y: 0.2 })
                                    .build();
                            }
                        });
                        engine.process_frame().unwrap();
                        black_box(engine.world().entity_count());
                    },
                    criterion::BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

/// Pre-spawns `count` entities, then queues them all for destruction via `Commands.destroy_entity()`.
/// Measures deferred destruction throughput including command execution and free-list recycling.
fn bench_commands_destroy_entity(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("commands_destroy_entity");
    for &count in &[100, 1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                benchmark.iter_batched(
                    || {
                        // Step 1: Pre-spawn the entities the despawner system will destroy.
                        let mut engine = Engine::new();
                        engine.world_mut().register_component::<Position>();
                        for i in 0..count {
                            engine
                                .world_mut()
                                .create_entity()
                                .with(Position {
                                    x: i as f32,
                                    y: 0.0,
                                })
                                .build()
                                .unwrap();
                        }
                        engine
                    },
                    |mut engine| {
                        // Step 2: Register the system that queues every live entity for destruction.
                        engine.register_system(
                            "despawner",
                            move |mut query: Query<Entity>, mut commands: Commands| {
                                for entity in query.iter_mut() {
                                    commands.destroy_entity(entity);
                                }
                            },
                        );
                        // Step 3: Flush the queue and measure whether the frame succeeded.
                        black_box(engine.process_frame().is_ok());
                    },
                    criterion::BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

/// Pre-spawns `count` single-component entities, then queues `add_component_to_entity(Health)` for all.
/// Measures the deferred archetype migration path through the command queue.
fn bench_commands_add_component(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("commands_add_component");
    for &count in &[100, 1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                benchmark.iter_batched(
                    || {
                        // Step 1: Pre-spawn the entities that will gain a `Health` component.
                        let mut engine = Engine::new();
                        engine.world_mut().register_component::<Position>();
                        engine.world_mut().register_component::<Health>();
                        for i in 0..count {
                            engine
                                .world_mut()
                                .create_entity()
                                .with(Position {
                                    x: i as f32,
                                    y: 0.0,
                                })
                                .build()
                                .unwrap();
                        }
                        engine
                    },
                    |mut engine| {
                        // Step 2: Register the system that queues a `Health` addition per entity.
                        engine.register_system(
                            "adder",
                            move |mut query: Query<(Entity, &Position)>, mut commands: Commands| {
                                for (entity, _position) in query.iter_mut() {
                                    commands.add_component_to_entity(entity, Health(100.0));
                                }
                            },
                        );
                        // Step 3: Flush the queue and measure whether the frame succeeded.
                        black_box(engine.process_frame().is_ok());
                    },
                    criterion::BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

// =============================================================================
// Benchmark Harness
// =============================================================================

criterion_group!(
    benches,
    bench_resource_insert,
    bench_resource_get,
    bench_resource_get_mut,
    bench_resource_remove,
    bench_commands_create_entity,
    bench_commands_destroy_entity,
    bench_commands_add_component,
);
criterion_main!(benches);
