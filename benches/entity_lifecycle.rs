//! Entity Lifecycle Benchmarks
//! ============================
//!
//! Entity creation and destruction are the most fundamental ECS operations —
//! every spawned enemy, bullet, or particle pays these costs. These benchmarks
//! isolate the allocation paths that live outside the hot query loop.
//!
//! What each benchmark isolates:
//!
//! - **Create**: fresh `World`, no pre-allocation — raw allocation + HashMap
//!   insertion + Vec growth.
//! - **Create (reserved)**: calls `world.reserve_entities(count)` first to
//!   pre-allocate internal structures, showing how much overhead is avoidable.
//! - **Destroy**: tears down single-component entities one at a time, measuring
//!   HashMap removal, free-list push, and empty-archetype cleanup.
//! - **Reuse cycle**: create → destroy → create again, exercising the free-list
//!   ID recycling path vs allocating brand-new IDs.
//! - **Many components**: 6 components per entity in a single wide archetype,
//!   measuring the added cost of building large-component entities.

#![allow(dead_code)]

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use ecs_hybrid::*;
use trait_type_map::impl_trait_accessible;

#[derive(Debug, Clone)]
struct Transform { x: f32, y: f32, z: f32 }
impl Component for Transform {}

#[derive(Debug, Clone)]
struct Velocity { x: f32, y: f32, z: f32 }
impl Component for Velocity {}

#[derive(Debug, Clone)]
struct Health(f32);
impl Component for Health {}

impl_trait_accessible!(dyn Component; Transform, Velocity, Health);

fn create_entities(world: &mut World, count: usize) {
    for _ in 0..count {
        world
            .create_entity()
            .with(Transform { x: 1.0, y: 2.0, z: 3.0 })
            .with(Velocity { x: 0.1, y: 0.2, z: 0.3 })
            .with(Health(100.0))
            .build()
            .unwrap();
    }
}

/// Creates `count` entities with 3 components each in a fresh `World`.
/// Measures allocation overhead, HashMap insertion, and Vec growth during bulk spawning.
fn bench_create(c: &mut Criterion) {
    let mut group = c.benchmark_group("entity_create");
    for &count in &[100, 1_000, 10_000] {
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter(|| {
                let mut world = World::new();
                world.register_component::<Transform>();
                world.register_component::<Velocity>();
                world.register_component::<Health>();
                create_entities(&mut world, count);
                black_box(world.entity_count());
            });
        });
    }
    group.finish();
}

/// Same as `bench_create` but calls `world.reserve_entities(count)` first.
/// Measures how much pre-allocation reduces reallocation overhead vs the non-reserved path.
fn bench_create_reserved(c: &mut Criterion) {
    let mut group = c.benchmark_group("entity_create_reserved");
    for &count in &[1_000, 10_000, 100_000] {
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter(|| {
                let mut world = World::new();
                world.register_component::<Transform>();
                world.register_component::<Velocity>();
                world.register_component::<Health>();
                world.reserve_entities(count);
                create_entities(&mut world, count);
                black_box(world.entity_count());
            });
        });
    }
    group.finish();
}

/// Destroys `count` single-component entities one by one.
/// Measures HashMap removal cost, free-list push overhead, and archetype cleanup.
fn bench_destroy(c: &mut Criterion) {
    let mut group = c.benchmark_group("entity_destroy");
    for &count in &[100, 1_000, 10_000] {
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter_batched(
                || {
                    let mut world = World::new();
                    world.register_component::<Transform>();
                    let mut handles = Vec::with_capacity(count);
                    for i in 0..count {
                        handles.push(
                            world
                                .create_entity()
                                .with(Transform { x: i as f32, y: 0.0, z: 0.0 })
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
        });
    }
    group.finish();
}

/// Creates entities, destroys all of them, then immediately creates the same number again.
/// Exercises the free-list ID recycling path to measure reuse vs fresh-allocation overhead.
fn bench_reuse_cycle(c: &mut Criterion) {
    let mut group = c.benchmark_group("entity_reuse_cycle");
    for &cycle_count in &[100, 1_000, 10_000] {
        group.bench_with_input(BenchmarkId::from_parameter(cycle_count), &cycle_count, |b, &cycle_count| {
            b.iter_batched(
                || {
                    let mut world = World::new();
                    world.register_component::<Transform>();
                    world.register_component::<Velocity>();
                    world
                },
                |mut world| {
                    // Create entities
                    let mut handles = Vec::with_capacity(cycle_count);
                    for i in 0..cycle_count {
                        handles.push(
                            world.create_entity()
                                .with(Transform { x: i as f32, y: 0.0, z: 0.0 })
                                .with(Velocity { x: 0.1, y: 0.2, z: 0.3 })
                                .build().unwrap(),
                        );
                    }
                    // Destroy all
                    for &entity in &handles {
                        let _ = world.destroy_entity(entity);
                    }
                    // Re-create (should reuse freed IDs via the free list)
                    for i in 0..cycle_count {
                        black_box(
                            world.create_entity()
                                .with(Transform { x: i as f32, y: 0.0, z: 0.0 })
                                .with(Velocity { x: 0.1, y: 0.2, z: 0.3 })
                                .build().unwrap(),
                        );
                    }
                },
                criterion::BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

/// Creates entities with 6 components each, forming a single wide archetype.
/// Measures the cost of building large-archetype entities vs the 3-component baseline.
fn bench_create_many_components(c: &mut Criterion) {
    let mut group = c.benchmark_group("entity_create_many_components");

    // Define additional components locally
    #[derive(Debug, Clone)]
    struct Armor(f32);
    impl Component for Armor {}
    #[derive(Debug, Clone)]
    struct Mana(f32);
    impl Component for Mana {}
    #[derive(Debug, Clone)]
    struct Stamina(f32);
    impl Component for Stamina {}
    impl_trait_accessible!(dyn Component; Armor, Mana, Stamina);

    for &count in &[100, 1_000, 10_000] {
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter(|| {
                let mut world = World::new();
                world.register_component::<Transform>();
                world.register_component::<Velocity>();
                world.register_component::<Health>();
                world.register_component::<Armor>();
                world.register_component::<Mana>();
                world.register_component::<Stamina>();
                for i in 0..count {
                    world.create_entity()
                        .with(Transform { x: i as f32, y: 0.0, z: 0.0 })
                        .with(Velocity { x: 0.1, y: 0.2, z: 0.3 })
                        .with(Health(100.0))
                        .with(Armor(50.0))
                        .with(Mana(200.0))
                        .with(Stamina(150.0))
                        .build().unwrap();
                }
                black_box(world.entity_count());
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_create, bench_create_reserved, bench_destroy, bench_reuse_cycle, bench_create_many_components);
criterion_main!(benches);
