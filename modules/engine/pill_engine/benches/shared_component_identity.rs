//! Shared component identity benchmarks: whether finding a component by its
//! declared identity instead of by `TypeId` costs anything per row.
//!
//! # Responsibilities
//!
//! - Compares query iteration over a shared component against an otherwise
//!   identical ordinary one, at three entity counts.
//! - Covers the read (`&T`), write (`&mut T`) and random-access
//!   (`get_component`) paths. The first two bind the column once per
//!   archetype, and random access resolves it again on every call.
//! - Measures reading a shared component through the other binary's copy of
//!   the type, the case the shared identity exists for.
//!
//! # Design
//!
//! [`SharedPosition`] and [`OrdinaryPosition`] have identical layouts. Only
//! the first declares `#[pill(shared = "bench::SharedPosition")]`.
//! [`ForeignPosition`] declares the same shared name, standing in for the copy
//! a second binary would compile. That is how one benchmark binary can reach
//! the cross-binary read path.
//!
//! Row access should be identical, not merely close. The shared type and the
//! foreign copy index one contiguous `ComponentColumn`: reads go through
//! `get_unchecked::<T>(row)` and writes through `get_mut_unchecked::<T>(row)`,
//! so no part of the identity reaches the loop body. Identity is resolved when
//! a query binds its columns and on every `get_component` call. An identity
//! derived from a declared shared name is a compile-time constant, so it costs
//! the same as `TypeId::of`: one load. A gap that grows with the entity count
//! means the row path changed, and that is the regression this target exists
//! to catch.

// External crates
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

// Current crate
use pill_engine::query::Query;
use pill_engine::{PillComponent, World};

// =============================================================================
// Entity Counts
// =============================================================================

/// The entity counts the two iteration benchmarks sweep.
///
/// They match the sequential groups in `query_iteration`, so the two targets'
/// numbers can be compared. `bench_random_access` measures the middle count
/// only.
const ENTITY_COUNTS: [usize; 3] = [1_000, 10_000, 100_000];

// =============================================================================
// Components
// =============================================================================

/// A component that declares a shared identity: every binary linking it uses
/// the same id for this component, so their rows land in one column.
#[derive(Clone, Copy, Debug, Default, PillComponent)]
#[pill(shared = "bench::SharedPosition")]
#[repr(C)]
struct SharedPosition {
    x: f32,
    y: f32,
}

/// A second declaration of the same shared component: a different Rust type
/// with the same identity, standing in for the copy another binary would
/// compile.
#[derive(Clone, Copy, Debug, Default, PillComponent)]
#[pill(shared = "bench::SharedPosition")]
#[repr(C)]
struct ForeignPosition {
    x: f32,
    y: f32,
}

/// The control: the same fields and derives, with no shared identity, so it
/// keeps the ordinary per-binary `TypeId`.
#[derive(Clone, Copy, Debug, Default, PillComponent)]
#[repr(C)]
struct OrdinaryPosition {
    x: f32,
    y: f32,
}

/// A second component so the iteration benchmarks fetch a pair, as the
/// sequential benchmarks in `query_iteration` do.
#[derive(Clone, Copy, Debug, Default, PillComponent)]
#[repr(C)]
struct Velocity {
    x: f32,
    y: f32,
}

// =============================================================================
// Worlds
// =============================================================================

/// Builds a world of `entity_count` entities carrying a shared position and a
/// velocity.
///
/// [`ForeignPosition`] is registered too, standing in for a second binary that
/// runs its own startup. The registry refuses that registration, because two
/// differently named Rust types claiming one shared name is the mistake it
/// guards against. Nothing here depends on that: both types resolve to the
/// same component id, so they read and write one column either way.
fn setup_shared_world(entity_count: usize) -> World {
    // Step 1: Register the shared component, its other copy, and `Velocity`.
    let mut world = World::new();
    world.register_component::<SharedPosition>();
    world.register_component::<ForeignPosition>();
    world.register_component::<Velocity>();

    // Step 2: Spawn `entity_count` entities with fixed component values.
    for index in 0..entity_count {
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

/// Builds the control world: the same layout and the same values, with no
/// declared shared identity.
fn setup_ordinary_world(entity_count: usize) -> World {
    // Step 1: Register the ordinary component and `Velocity`.
    let mut world = World::new();
    world.register_component::<OrdinaryPosition>();
    world.register_component::<Velocity>();

    // Step 2: Spawn `entity_count` entities with the values the shared world
    // uses.
    for index in 0..entity_count {
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

/// Reads a position and velocity pair in sequence over three types: the
/// ordinary component, the shared one, and the other binary's copy of the
/// shared one. All three series should be indistinguishable.
fn bench_read_iteration(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("shared_identity_read");
    for &count in &ENTITY_COUNTS {
        group.bench_with_input(
            BenchmarkId::new("ordinary", count),
            &count,
            |benchmark, &count| {
                let mut world = setup_ordinary_world(count);
                benchmark.iter(|| {
                    let mut query = Query::<(&OrdinaryPosition, &Velocity)>::new(&mut world);
                    let mut sum_x: f32 = 0.0;
                    for (position, velocity) in query.iter_mut() {
                        sum_x += position.x + velocity.x;
                    }
                    black_box(sum_x);
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("shared", count),
            &count,
            |benchmark, &count| {
                let mut world = setup_shared_world(count);
                benchmark.iter(|| {
                    let mut query = Query::<(&SharedPosition, &Velocity)>::new(&mut world);
                    let mut sum_x: f32 = 0.0;
                    for (position, velocity) in query.iter_mut() {
                        sum_x += position.x + velocity.x;
                    }
                    black_box(sum_x);
                });
            },
        );
        // The case the feature exists for: the same rows, read through the
        // other binary's copy of the type. Both types resolve to one column,
        // so this series should match the two above exactly.
        group.bench_with_input(
            BenchmarkId::new("shared_via_other_copy", count),
            &count,
            |benchmark, &count| {
                let mut world = setup_shared_world(count);
                benchmark.iter(|| {
                    let mut query = Query::<(&ForeignPosition, &Velocity)>::new(&mut world);
                    let mut sum_x: f32 = 0.0;
                    for (position, velocity) in query.iter_mut() {
                        sum_x += position.x + velocity.x;
                    }
                    black_box(sum_x);
                });
            },
        );
    }
    group.finish();
}

/// Writes a position while reading a velocity, in sequence, over the ordinary
/// and the shared component.
///
/// Every row is written, so each write also stores that row's `changed` tick
/// in the column's tick vector.
fn bench_write_iteration(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("shared_identity_write");
    for &count in &ENTITY_COUNTS {
        group.bench_with_input(
            BenchmarkId::new("ordinary", count),
            &count,
            |benchmark, &count| {
                let mut world = setup_ordinary_world(count);
                benchmark.iter(|| {
                    let mut query = Query::<(&mut OrdinaryPosition, &Velocity)>::new(&mut world);
                    for (mut position, velocity) in query.iter_mut() {
                        position.x += velocity.x;
                        position.y += velocity.y;
                    }
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("shared", count),
            &count,
            |benchmark, &count| {
                let mut world = setup_shared_world(count);
                benchmark.iter(|| {
                    let mut query = Query::<(&mut SharedPosition, &Velocity)>::new(&mut world);
                    for (mut position, velocity) in query.iter_mut() {
                        position.x += velocity.x;
                        position.y += velocity.y;
                    }
                });
            },
        );
    }
    group.finish();
}

/// Random access through [`World::get_component`], which looks the column up
/// again on every call instead of binding it once per archetype. That makes it
/// the path most likely to show a difference in how identity is computed.
fn bench_random_access(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("shared_identity_get_component");
    const ENTITY_COUNT: usize = 10_000;

    group.bench_function("ordinary", |benchmark| {
        let world = setup_ordinary_world(ENTITY_COUNT);
        let entities: Vec<_> = world
            .archetypes_iter()
            .flat_map(|archetype| archetype.entities.iter().copied())
            .collect();
        benchmark.iter(|| {
            let mut sum_x: f32 = 0.0;
            for &entity in &entities {
                if let Some(position) = world.get_component::<OrdinaryPosition>(entity) {
                    sum_x += position.x;
                }
            }
            black_box(sum_x);
        });
    });

    group.bench_function("shared", |benchmark| {
        let world = setup_shared_world(ENTITY_COUNT);
        let entities: Vec<_> = world
            .archetypes_iter()
            .flat_map(|archetype| archetype.entities.iter().copied())
            .collect();
        benchmark.iter(|| {
            let mut sum_x: f32 = 0.0;
            for &entity in &entities {
                if let Some(position) = world.get_component::<SharedPosition>(entity) {
                    sum_x += position.x;
                }
            }
            black_box(sum_x);
        });
    });

    group.bench_function("shared_via_other_copy", |benchmark| {
        let world = setup_shared_world(ENTITY_COUNT);
        let entities: Vec<_> = world
            .archetypes_iter()
            .flat_map(|archetype| archetype.entities.iter().copied())
            .collect();
        benchmark.iter(|| {
            let mut sum_x: f32 = 0.0;
            for &entity in &entities {
                if let Some(position) = world.get_component::<ForeignPosition>(entity) {
                    sum_x += position.x;
                }
            }
            black_box(sum_x);
        });
    });

    group.finish();
}

// =============================================================================
// Criterion Entry Point
// =============================================================================

criterion_group!(
    benches,
    bench_read_iteration,
    bench_write_iteration,
    bench_random_access
);
criterion_main!(benches);
