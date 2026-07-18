//! Archetype migration benchmarks - add/remove component throughput.
//!
//! Measures: clone + move overhead, HashMap insertion, Vec reallocation
//! during entity archetype transitions.

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

#[derive(Debug, Clone)]
struct Armor(f32);
impl Component for Armor {}

impl_trait_accessible!(dyn Component; Position, Velocity, Health, Armor);

fn setup_world(entity_count: usize) -> (World, Vec<Entity>) {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();
    world.register_component::<Armor>();

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
    (world, handles)
}

fn bench_add_component(c: &mut Criterion) {
    let mut group = c.benchmark_group("archetype_add_component");
    for &count in &[1_000, 10_000] {
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter_batched(
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
        });
    }
    group.finish();
}

fn bench_remove_component(c: &mut Criterion) {
    let mut group = c.benchmark_group("archetype_remove_component");
    for &count in &[1_000, 10_000] {
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter_batched(
                || {
                    let (mut world, handles) = setup_world(count);
                    // Add Health first so we can remove it later.
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
        });
    }
    group.finish();
}

criterion_group!(benches, bench_add_component, bench_remove_component);
criterion_main!(benches);
