//! Query iteration benchmarks — sequential + parallel, filtered + unfiltered.
//!
//! Measures: per-row overhead, cache behaviour, parallel scaling,
//! change-detection tick overhead, filter evaluation cost.

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

impl_trait_accessible!(dyn Component; Position, Velocity, Health);

fn setup_world(entity_count: usize) -> World {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();

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

// ── sequential, unfiltered ──────────────────────────────────────────

fn bench_iter_unfiltered(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_iter_unfiltered");
    for &count in &[1_000, 10_000, 100_000] {
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            let mut world = setup_world(count);
            b.iter(|| {
                let mut q = Query::<(&Position, &Velocity)>::new(&mut world);
                let mut sum_x: f32 = 0.0;
                for (pos, vel) in q.iter_mut() {
                    sum_x += pos.x + vel.x;
                }
                black_box(sum_x);
            });
        });
    }
    group.finish();
}

// ── sequential, mutable ─────────────────────────────────────────────

fn bench_iter_mutable(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_iter_mutable");
    for &count in &[1_000, 10_000, 100_000] {
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            let mut world = setup_world(count);
            b.iter(|| {
                let mut q = Query::<(&mut Position, &Velocity)>::new(&mut world);
                for (mut pos, vel) in q.iter_mut() {
                    pos.x += vel.x;
                    pos.y += vel.y;
                }
            });
        });
    }
    group.finish();
}

// ── sequential, with change-detection filter ────────────────────────

fn bench_iter_changed(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_iter_changed");
    for &count in &[1_000, 10_000, 100_000] {
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            let mut world = setup_world(count);
            // First frame: mutate all positions so Changed<Position> fires.
            {
                let mut q = Query::<&mut Position>::new(&mut world);
                for mut pos in q.iter_mut() {
                    pos.x += 1.0;
                }
            }
            world.increment_change_tick();
            b.iter(|| {
                let mut q = Query::<(&Position,), Changed<Position>>::new(&mut world);
                let mut count = 0usize;
                for (pos,) in q.iter_mut() {
                    black_box(pos.x);
                    count += 1;
                }
                black_box(count);
            });
        });
    }
    group.finish();
}

// ── parallel, unfiltered ────────────────────────────────────────────

fn bench_par_iter_unfiltered(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_par_iter_unfiltered");
    for &count in &[10_000, 100_000, 1_000_000] {
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            let mut world = setup_world(count);
            b.iter(|| {
                let mut q = Query::<(&Position, &Velocity)>::new(&mut world);
                q.par_iter_mut().for_each(|(pos, vel)| {
                    black_box(pos.x + vel.x);
                });
            });
        });
    }
    group.finish();
}

// ── helper: count, is_empty, first ──────────────────────────────────

fn bench_query_helpers(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_helpers");
    let count = 10_000;

    group.bench_function("entity_count_unfiltered", |b| {
        let mut world = setup_world(count);
        b.iter(|| {
            let mut q = Query::<(&Position,)>::new(&mut world);
            black_box(q.entity_count());
        });
    });

    group.bench_function("is_empty_unfiltered", |b| {
        let mut world = setup_world(count);
        b.iter(|| {
            let mut q = Query::<(&Position,)>::new(&mut world);
            black_box(q.is_empty());
        });
    });

    group.bench_function("first_unfiltered", |b| {
        let mut world = setup_world(count);
        b.iter(|| {
            let mut q = Query::<(&Position,)>::new(&mut world);
            black_box(q.first());
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_iter_unfiltered,
    bench_iter_mutable,
    bench_iter_changed,
    bench_par_iter_unfiltered,
    bench_query_helpers,
);
criterion_main!(benches);
