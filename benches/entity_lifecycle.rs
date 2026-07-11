//! Entity lifecycle benchmarks — create + destroy at scale.
//!
//! Measures: allocation overhead, HashMap insertion, Vec growth,
//! archetype creation, free-list behaviour.

#![allow(dead_code)]

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use ecs_hybrid::*;
use trait_type_map::impl_trait_accessible;

#[derive(Debug, Clone)]
struct Transform {
    x: f32,
    y: f32,
    z: f32,
}
impl Component for Transform {}

#[derive(Debug, Clone)]
struct Velocity {
    x: f32,
    y: f32,
    z: f32,
}
impl Component for Velocity {}

impl_trait_accessible!(dyn Component; Transform, Velocity);

fn create_entities(count: usize) -> World {
    let mut world = World::new();
    world.register_component::<Transform>();
    world.register_component::<Velocity>();

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
            .build()
            .unwrap();
    }
    world
}

fn bench_create(c: &mut Criterion) {
    let mut group = c.benchmark_group("entity_create");
    for &count in &[100, 1_000, 10_000] {
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter(|| create_entities(black_box(count)));
        });
    }
    group.finish();
}

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
        });
    }
    group.finish();
}

criterion_group!(benches, bench_create, bench_destroy);
criterion_main!(benches);
