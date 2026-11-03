//! The smoke-test benchmark: one hot path, one entity count, fast to run.
//!
//! Run it with `cargo bench --bench minimal -- --quick` as a check that the
//! benchmark harness itself works. It is deliberately not a coverage target:
//! `query_iteration.rs` covers the iteration variants and
//! `archetype_migration.rs` covers migration, so a case added here would only
//! repeat one of them at a single fixed size.
//!
//! # Responsibilities
//!
//! - Benchmarks sequential unfiltered query iteration at a fixed size.
//! - Defines the [`Position`] and [`Velocity`] component types it uses.
//!
//! # Design
//!
//! The world is built once per run, before the first measurement, with
//! [`QUERY_ENTITY_COUNT`] entities that all carry [`Position`] and
//! [`Velocity`]. So every run starts from the same world, and the measured
//! work is creating the query and iterating it, not creating the entities.

// External crates
use criterion::{black_box, criterion_group, criterion_main, Criterion};

// Current crate
use pill_engine::{Component, Entity, Query, World};

// =============================================================================
// Constants
// =============================================================================

/// Number of entities the benchmark's world is populated with.
const QUERY_ENTITY_COUNT: usize = 100_000;

// =============================================================================
// Position
// =============================================================================

/// Two-dimensional position component used by the query benchmark.
///
/// It holds two `f32` values instead of one, so a pass over it reads more
/// memory per entity than a one-value component does.
#[derive(Clone)]
struct Position {
    /// Horizontal coordinate in world units.
    x: f32,
    /// Vertical coordinate in world units.
    y: f32,
}
impl Component for Position {}

// =============================================================================
// Velocity
// =============================================================================

/// Two-dimensional velocity component used by the query benchmark.
///
/// Read alongside [`Position`] so the unfiltered query iterates two components
/// per entity.
#[derive(Clone)]
struct Velocity {
    /// Horizontal velocity in world units per second.
    x: f32,
    /// Vertical velocity in world units per second.
    y: f32,
}
impl Component for Velocity {}

// =============================================================================
// Free Functions
// =============================================================================

/// Builds a fresh [`World`](pill_engine::World) holding `entity_count`
/// entities.
///
/// Every entity carries [`Position`] and [`Velocity`], so all of them fall
/// into the one archetype the unfiltered query reads.
///
/// Returns the world and the handles of the entities it created.
///
/// # Panics
///
/// Panics if the world refuses to build an entity. Both component types are
/// registered first, so a failure here means the fixture itself is wrong.
fn setup_world(entity_count: usize) -> (World, Vec<Entity>) {
    // Step 1: Create the world and register every component type.
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();

    // Step 2: Create `entity_count` entities, each carrying Position and Velocity.
    let mut entities = Vec::with_capacity(entity_count);
    for index in 0..entity_count {
        entities.push(
            world
                .create_entity()
                .with(Position {
                    x: index as f32,
                    y: index as f32 * 2.0,
                })
                .with(Velocity { x: 0.1, y: 0.2 })
                .build()
                .unwrap(),
        );
    }
    (world, entities)
}

/// Benchmarks unfiltered sequential iteration over every entity.
///
/// The accumulated sum is passed through [`black_box`] so the compiler cannot
/// elide the loop or its reads.
fn bench_unfiltered_query_iteration(criterion: &mut Criterion) {
    let (mut world, _) = setup_world(QUERY_ENTITY_COUNT);
    criterion.bench_function("query_iter_unfiltered", |benchmark| {
        benchmark.iter(|| {
            let mut sum = 0.0;
            let mut query = Query::<(&Position, &Velocity)>::new(&mut world);
            for (position, velocity) in query.iter_mut() {
                sum += position.x + position.y + velocity.x + velocity.y;
            }
            black_box(sum);
        });
    });
}

// =============================================================================
// Benchmark Registration
// =============================================================================

criterion_group!(benches, bench_unfiltered_query_iteration);
criterion_main!(benches);
