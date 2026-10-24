//! Shared Component Identity Benchmarks
//! ===================================
//!
//! Verifies the claim that resolving a component by a declared identity rather
//! than by `TypeId` costs nothing in the inner loop, instead of assuming it.
//!
//! # Responsibilities
//!
//! - Compares query iteration over a shared component against an otherwise
//!   identical ordinary one, at three entity counts.
//! - Covers the read (`&T`), write (`&mut T`) and random-access
//!   (`get_component`) paths, since they resolve the column differently.
//! - Measures reading a shared component through the *other* binary's copy of
//!   the type, which is the case the feature exists for.
//!
//! # Design
//!
//! `SharedPosition` and `OrdinaryPosition` have identical layouts and differ
//! only in that the first declares `#[pill(shared)]`. `ForeignPosition` is a
//! third type declaring the same shared name as `SharedPosition`, standing in
//! for the copy a second binary would compile - which is how the cross-binary
//! read path is reachable from a single benchmark binary.
//!
//! ## What the numbers should show
//!
//! Row access is expected to be *identical*, not merely close: both kinds of
//! column are the same contiguous `ComponentColumn`, and both are indexed by
//! `get_unchecked::<T>(row)`, which is a base pointer plus `row * size_of::<T>()`.
//! Nothing about the declared identity reaches the loop body.
//!
//! The only place identity is consulted is once per query, where a shared
//! component's id comes from hashing its declared name instead of from
//! `TypeId::of`. Both fold to a constant at compile time, so even that should
//! not be visible above noise. A gap that scales with entity count would mean
//! the row path diverged and is the regression this benchmark exists to catch.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use pill_engine::query::Query;
use pill_engine::{PillComponent, World};

// =============================================================================
// Components
// =============================================================================

/// A component with a declared cross-binary identity.
#[derive(Clone, Copy, Debug, Default, PillComponent)]
#[pill(shared = "bench::SharedPosition")]
#[repr(C)]
struct SharedPosition {
    x: f32,
    y: f32,
}

/// A second declaration of the same shared component, standing in for the copy
/// another binary would compile: a different Rust type, the same identity.
#[derive(Clone, Copy, Debug, Default, PillComponent)]
#[pill(shared = "bench::SharedPosition")]
#[repr(C)]
struct ForeignPosition {
    x: f32,
    y: f32,
}

/// The control: identical in every respect except that it declares no shared
/// identity, so it keeps the ordinary per-binary `TypeId`.
#[derive(Clone, Copy, Debug, Default, PillComponent)]
#[repr(C)]
struct OrdinaryPosition {
    x: f32,
    y: f32,
}

/// A second component so every query fetches a pair, matching the shape of the
/// sequential benchmarks in `query_iteration`.
#[derive(Clone, Copy, Debug, Default, PillComponent)]
#[repr(C)]
struct Velocity {
    x: f32,
    y: f32,
}

// =============================================================================
// Worlds
// =============================================================================

/// Entity counts every benchmark sweeps, matching `query_iteration` so the
/// numbers are comparable against the ordinary query benchmarks.
const ENTITY_COUNTS: [usize; 3] = [1_000, 10_000, 100_000];

/// Build a world of `count` entities carrying a shared position and a velocity.
///
/// Both copies of the shared component register, as they would when a project
/// and a module DLL each run their own `init`, so the column is the bound one
/// rather than a single-registrant special case.
fn setup_shared_world(count: usize) -> World {
    let mut world = World::new();
    world.register_component::<SharedPosition>();
    world.register_component::<ForeignPosition>();
    world.register_component::<Velocity>();
    for index in 0..count {
        world
            .create_entity()
            .with(SharedPosition {
                x: index as f32,
                y: (index * 2) as f32,
            })
            .with(Velocity { x: 0.1, y: 0.2 })
            .build()
            .unwrap();
    }
    world
}

/// Build the control world, identical but for the component's identity.
fn setup_ordinary_world(count: usize) -> World {
    let mut world = World::new();
    world.register_component::<OrdinaryPosition>();
    world.register_component::<Velocity>();
    for index in 0..count {
        world
            .create_entity()
            .with(OrdinaryPosition {
                x: index as f32,
                y: (index * 2) as f32,
            })
            .with(Velocity { x: 0.1, y: 0.2 })
            .build()
            .unwrap();
    }
    world
}

// =============================================================================
// Benchmarks
// =============================================================================

/// Sequential read of `(&Position, &Velocity)` over a shared component against
/// an ordinary one. The two series should be indistinguishable.
fn bench_read_iteration(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("shared_identity_read");
    for &count in &ENTITY_COUNTS {
        group.bench_with_input(BenchmarkId::new("ordinary", count), &count, |bench, &n| {
            let mut world = setup_ordinary_world(n);
            bench.iter(|| {
                let mut query = Query::<(&OrdinaryPosition, &Velocity)>::new(&mut world);
                let mut sum: f32 = 0.0;
                for (position, velocity) in query.iter_mut() {
                    sum += position.x + velocity.x;
                }
                black_box(sum);
            });
        });
        group.bench_with_input(BenchmarkId::new("shared", count), &count, |bench, &n| {
            let mut world = setup_shared_world(n);
            bench.iter(|| {
                let mut query = Query::<(&SharedPosition, &Velocity)>::new(&mut world);
                let mut sum: f32 = 0.0;
                for (position, velocity) in query.iter_mut() {
                    sum += position.x + velocity.x;
                }
                black_box(sum);
            });
        });
        // The case the feature exists for: the same rows read through the
        // other binary's copy of the type. It resolves to the same column, so
        // it should match the series above exactly.
        group.bench_with_input(
            BenchmarkId::new("shared_via_other_copy", count),
            &count,
            |bench, &n| {
                let mut world = setup_shared_world(n);
                bench.iter(|| {
                    let mut query = Query::<(&ForeignPosition, &Velocity)>::new(&mut world);
                    let mut sum: f32 = 0.0;
                    for (position, velocity) in query.iter_mut() {
                        sum += position.x + velocity.x;
                    }
                    black_box(sum);
                });
            },
        );
    }
    group.finish();
}

/// Sequential mutable iteration, which additionally writes a change tick per
/// row through the tick vector the column's id keys.
fn bench_write_iteration(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("shared_identity_write");
    for &count in &ENTITY_COUNTS {
        group.bench_with_input(BenchmarkId::new("ordinary", count), &count, |bench, &n| {
            let mut world = setup_ordinary_world(n);
            bench.iter(|| {
                let mut query = Query::<(&mut OrdinaryPosition, &Velocity)>::new(&mut world);
                for (mut position, velocity) in query.iter_mut() {
                    position.x += velocity.x;
                    position.y += velocity.y;
                }
            });
        });
        group.bench_with_input(BenchmarkId::new("shared", count), &count, |bench, &n| {
            let mut world = setup_shared_world(n);
            bench.iter(|| {
                let mut query = Query::<(&mut SharedPosition, &Velocity)>::new(&mut world);
                for (mut position, velocity) in query.iter_mut() {
                    position.x += velocity.x;
                    position.y += velocity.y;
                }
            });
        });
    }
    group.finish();
}

/// Random access through `get_component`, which resolves the column once per
/// call rather than once per archetype - the path most exposed to a difference
/// in how identity is computed.
fn bench_random_access(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("shared_identity_get_component");
    const COUNT: usize = 10_000;

    group.bench_function("ordinary", |bench| {
        let world = setup_ordinary_world(COUNT);
        let entities: Vec<_> = world
            .archetypes_iter()
            .flat_map(|archetype| archetype.entities.iter().copied())
            .collect();
        bench.iter(|| {
            let mut sum: f32 = 0.0;
            for &entity in &entities {
                if let Some(position) = world.get_component::<OrdinaryPosition>(entity) {
                    sum += position.x;
                }
            }
            black_box(sum);
        });
    });

    group.bench_function("shared", |bench| {
        let world = setup_shared_world(COUNT);
        let entities: Vec<_> = world
            .archetypes_iter()
            .flat_map(|archetype| archetype.entities.iter().copied())
            .collect();
        bench.iter(|| {
            let mut sum: f32 = 0.0;
            for &entity in &entities {
                if let Some(position) = world.get_component::<SharedPosition>(entity) {
                    sum += position.x;
                }
            }
            black_box(sum);
        });
    });

    group.bench_function("shared_via_other_copy", |bench| {
        let world = setup_shared_world(COUNT);
        let entities: Vec<_> = world
            .archetypes_iter()
            .flat_map(|archetype| archetype.entities.iter().copied())
            .collect();
        bench.iter(|| {
            let mut sum: f32 = 0.0;
            for &entity in &entities {
                if let Some(position) = world.get_component::<ForeignPosition>(entity) {
                    sum += position.x;
                }
            }
            black_box(sum);
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_read_iteration,
    bench_write_iteration,
    bench_random_access
);
criterion_main!(benches);
