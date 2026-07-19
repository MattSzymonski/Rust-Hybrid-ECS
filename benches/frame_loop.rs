//! End-to-end frame loop benchmark - realistic game-loop simulation.
//!
//! Measures: process_frame() wall-clock time under representative loads.
//! This is the ultimate metric - all micro-optimizations must eventually
//! show improvement here.

#![allow(dead_code)]

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use ecs_hybrid::*;
use trait_type_map::impl_trait_accessible;

#[derive(Debug, Clone)]
struct Position { x: f32, y: f32 }
impl Component for Position {}

#[derive(Debug, Clone)]
struct Velocity { x: f32, y: f32 }
impl Component for Velocity {}

#[derive(Debug, Clone)]
struct Health(f32);
impl Component for Health {}

/// 256 B component — stresses cache hierarchy
#[derive(Debug, Clone)]
struct MassiveData([[f64; 4]; 8]);
impl Component for MassiveData {}

impl_trait_accessible!(dyn Component; Position, Velocity, Health, MassiveData);

fn movement_system(mut query: Query<(&mut Position, &Velocity)>) {
    for (mut pos, vel) in query.iter_mut() {
        pos.x += vel.x;
        pos.y += vel.y;
    }
}

fn health_decay_system(mut query: Query<&mut Health>) {
    for mut health in query.iter_mut() {
        health.0 = (health.0 - 0.1).max(0.0);
    }
}

fn reporting_system(mut query: Query<(&Position, &Health)>) {
    let mut count = 0usize;
    let mut total_health: f32 = 0.0;
    for (_, health) in query.iter_mut() {
        total_health += health.0;
        count += 1;
    }
    black_box((count, total_health));
}

fn build_engine(entity_count: usize, use_large_component: bool) -> Engine {
    let mut engine = Engine::new();
    engine.set_parallel_execution(true);

    engine.world_mut().register_component::<Position>();
    engine.world_mut().register_component::<Velocity>();
    engine.world_mut().register_component::<Health>();
    if use_large_component {
        engine.world_mut().register_component::<MassiveData>();
    }

    for i in 0..entity_count {
        let mut builder = engine.world_mut().create_entity()
            .with(Position { x: i as f32, y: 0.0 })
            .with(Velocity { x: 0.1, y: 0.2 })
            .with(Health(100.0));
        if use_large_component {
            builder = builder.with(MassiveData([[i as f64; 4]; 8]));
        }
        builder.build().unwrap();
    }

    // System that reads the large component
    if use_large_component {
        engine.register_system("massive_read", |mut query: Query<(&MassiveData, &mut Health)>| {
            for (massive, mut health) in query.iter_mut() {
                health.0 += massive.0[0][0] as f32 * 0.001;
            }
        });
    }

    engine.register_system("movement", movement_system);
    engine.register_system("health_decay", health_decay_system);
    engine.register_system("reporting", reporting_system);

    engine
}

fn bench_frame_loop(c: &mut Criterion) {
    let mut group = c.benchmark_group("frame_loop");

    // Standard components (8-16B each) — baseline
    for &entity_count in &[100_000, 500_000] {
        group.bench_with_input(
            BenchmarkId::new("standard", entity_count),
            &entity_count,
            |b, &entity_count| {
                let mut engine = build_engine(entity_count, false);
                b.iter(|| { black_box(engine.process_frame().is_ok()); });
            },
        );
    }

    // With 256B MassiveData component — cache pressure test
    for &entity_count in &[100_000, 500_000] {
        group.bench_with_input(
            BenchmarkId::new("with_256B_component", entity_count),
            &entity_count,
            |b, &entity_count| {
                let mut engine = build_engine(entity_count, true);
                b.iter(|| { black_box(engine.process_frame().is_ok()); });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_frame_loop);
criterion_main!(benches);
