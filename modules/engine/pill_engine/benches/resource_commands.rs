//! Resource and commands benchmarks for the `pill_engine` world API.
//!
//! A resource is one value of its type - time, input, config - stored in the
//! `World` and read through `Res<T>` / `ResMut<T>` system parameters. Commands
//! are a queue of operations that a system schedules for later: create or
//! destroy an entity, add a component. The queue is applied after the system
//! runs, so the system itself never needs `&mut World`.
//!
//! # Responsibilities
//!
//! - Measures the four resource operations: insert (a map insert plus a heap
//!   allocation), get (an immutable lookup, then a downcast to the stored
//!   type), get_mut (a mutable lookup and downcast, with no change tick
//!   recorded) and remove (a map removal plus a deallocation).
//! - Measures the deferred-command pipeline: the operation is queued while the
//!   system runs and applied when the frame flushes the queue.
//! - Covers entity creation with two components, entity destruction, and
//!   component addition via `add_component_to_entity`.
//!
//! # Design
//!
//! The resource benchmarks build a fresh [`World`] and call its insert, read,
//! update and remove methods. The command benchmarks register a system that
//! queues work and then run `Engine::process_frame` to flush the queue. The
//! fixture component and resource types are defined here, so every benchmark
//! in this file uses the same declarations.

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
/// Fixture component for the three command benchmarks, which spawn entities
/// with it.
#[derive(Debug, Clone)]
struct Position {
    x: f32,
    y: f32,
}
impl Component for Position {}

/// Two-dimensional velocity of an entity in world space.
///
/// Fixture component used beside `Position` by the benchmark that creates
/// entities with two components through the command queue.
#[derive(Debug, Clone)]
struct Velocity {
    x: f32,
    y: f32,
}
impl Component for Velocity {}

/// Flat health value carried by an entity.
///
/// Fixture component added through the command queue by the benchmark that
/// measures `add_component_to_entity`.
#[derive(Debug, Clone)]
struct Health(f32);
impl Component for Health {}

// =============================================================================
// Resources
// =============================================================================

/// The project's clock, stored as a resource.
///
/// Holds the delta time of the current frame and the time elapsed since the
/// start. The resource insert, get, get_mut and remove benchmarks operate on
/// it.
#[derive(Debug)]
struct ProjectTime {
    delta_seconds: f32,
    elapsed_seconds: f32,
}
impl Resource for ProjectTime {}

/// Mouse position and button state, stored as a resource.
///
/// Part of the fixture set; no benchmark registers or measures it.
#[derive(Debug)]
struct InputState {
    mouse_x: f32,
    mouse_y: f32,
    buttons: u32,
}
impl Resource for InputState {}

/// Simulation settings, stored as a resource.
///
/// Part of the fixture set; no benchmark registers or measures it.
#[derive(Debug)]
struct Config {
    gravity: f32,
    max_entities: u32,
}
impl Resource for Config {}

// =============================================================================
// Benchmarks
// =============================================================================

/// Inserts the same `ProjectTime` resource `count` times into a fresh `World`.
///
/// Each insert replaces the previous value, so the last write wins. Measures
/// the map insert, the allocation of the new value and the free of the value
/// it replaced.
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
                            delta_seconds: 0.016,
                            elapsed_seconds: i as f32 * 0.016,
                        });
                    }
                    black_box(world.has_resource::<ProjectTime>());
                });
            },
        );
    }
    group.finish();
}

/// Reads the `ProjectTime` resource `count` times through
/// `world.get_resource()`, once per loop iteration.
///
/// Measures the read path for resources: a map lookup, then a downcast to the
/// stored type.
fn bench_resource_get(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("resource_get");
    for &count in &[1_000, 10_000, 100_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                let mut world = World::new();
                world.insert_resource(ProjectTime {
                    delta_seconds: 0.016,
                    elapsed_seconds: 0.0,
                });
                benchmark.iter(|| {
                    for _ in 0..count {
                        black_box(
                            world
                                .get_resource::<ProjectTime>()
                                .map(|resource| resource.elapsed_seconds),
                        );
                    }
                });
            },
        );
    }
    group.finish();
}

/// Mutates the `ProjectTime` resource `count` times through
/// `world.get_resource_mut()`.
///
/// Measures the mutable lookup and downcast path. This accessor records no
/// change tick; the tracked variant is `get_resource_mut_tracked`.
fn bench_resource_get_mut(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("resource_get_mut");
    for &count in &[1_000, 10_000, 100_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                let mut world = World::new();
                world.insert_resource(ProjectTime {
                    delta_seconds: 0.016,
                    elapsed_seconds: 0.0,
                });
                benchmark.iter(|| {
                    for _ in 0..count {
                        if let Some(project_time) = world.get_resource_mut::<ProjectTime>() {
                            project_time.elapsed_seconds += project_time.delta_seconds;
                            black_box(project_time.elapsed_seconds);
                        }
                    }
                });
            },
        );
    }
    group.finish();
}

/// Removes one `ProjectTime` resource from a `World` and drops it.
///
/// Measures the map removal and the deallocation of the stored value.
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
                                delta_seconds: 0.016,
                                elapsed_seconds: i as f32,
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

/// Queues `count` entity creations with two components each through
/// `Commands`, then executes the queue.
///
/// Measures the whole deferred-command pipeline: pushing each command, then
/// applying it in `process_frame`.
fn bench_commands_create_entity(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("commands_create_entity");
    for &count in &[100, 1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                // Build the engine outside the measurement. `Engine::new`
                // warms the Rayon pool and prints the parallel-iteration
                // settings, and that cost would otherwise dominate a small
                // run and hide the command pipeline the benchmark measures.
                benchmark.iter_batched(
                    || {
                        // Step 1: Setup - build a fresh engine with the
                        // component types the spawner writes. This runs
                        // outside the measurement.
                        let mut engine = Engine::new();
                        engine.world_mut().register_component::<Position>();
                        engine.world_mut().register_component::<Velocity>();
                        engine
                    },
                    |mut engine| {
                        // Step 2: Measured - register the spawning system,
                        // queue `count` creations through `Commands`, then
                        // flush the queue in `process_frame`.
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

/// Spawns `count` entities before the measurement, then queues all of them
/// for destruction through `Commands::destroy_entity`.
///
/// Measures deferred destruction: applying the queued commands, then
/// returning the freed ids to the free list.
fn bench_commands_destroy_entity(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("commands_destroy_entity");
    for &count in &[100, 1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                benchmark.iter_batched(
                    || {
                        // Step 1: Spawn the entities the despawner system will destroy.
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
                        // Step 2: Register the system that queues every live
                        // entity for destruction.
                        engine.register_system(
                            "despawner",
                            move |mut query: Query<Entity>, mut commands: Commands| {
                                for entity in query.iter_mut() {
                                    commands.destroy_entity(entity);
                                }
                            },
                        );
                        // Step 3: Flush the queue and pass the frame's result to
                        // `black_box`, so the measured work cannot be optimized
                        // away.
                        black_box(engine.process_frame().is_ok());
                    },
                    criterion::BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

/// Spawns `count` entities with one component each before the measurement,
/// then queues `add_component_to_entity(Health)` for every one of them.
///
/// Measures the deferred path that moves each entity to a new archetype.
fn bench_commands_add_component(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("commands_add_component");
    for &count in &[100, 1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &count,
            |benchmark, &count| {
                benchmark.iter_batched(
                    || {
                        // Step 1: Spawn the entities that will gain a `Health` component.
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
                        // Step 2: Register the system that queues one `Health`
                        // addition per entity.
                        engine.register_system(
                            "adder",
                            move |mut query: Query<(Entity, &Position)>, mut commands: Commands| {
                                for (entity, _position) in query.iter_mut() {
                                    commands.add_component_to_entity(entity, Health(100.0));
                                }
                            },
                        );
                        // Step 3: Flush the queue and pass the frame's result to
                        // `black_box`, so the measured work cannot be optimized
                        // away.
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
