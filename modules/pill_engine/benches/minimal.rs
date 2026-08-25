//! The smoke-test benchmark: one hot path, one size, fast to run.
//!
//! This is the target the documentation reaches for when it wants a quick check
//! that the benchmark harness itself works - `--bench minimal --quick`. It is
//! deliberately NOT a coverage target. `query_iteration.rs` measures query
//! iteration across sixteen groups and three entity counts, and
//! `archetype_migration.rs` covers migration; anything added here would
//! duplicate them at a single hardcoded size.
//!
//! It previously declared four benchmarks in its documentation while three of
//! them sat commented out of `criterion_group!`, so it measured a quarter of
//! what it claimed. Those three duplicated groups that the dedicated targets
//! already cover more thoroughly, and were removed rather than re-enabled.
//!
//! # Responsibilities
//!
//! - Benchmarks sequential unfiltered query iteration at a fixed size.
//! - Defines the [`Position`] and [`Velocity`] component types it uses.
//!
//! # Design
//!
//! Every benchmark builds a fresh [`World`](pill_engine::World) populated with
//! a fixed number of entities before measuring, so each run starts from
//! identical state. Change-detection benchmarks dirty every entity and bump
//! the world tick before measuring so the [`Changed`] filter matches the full
//! set. The archetype-migration benchmark uses Criterion's batched mode with
//! [`BatchSize::LargeInput`] to rebuild the world per iteration, keeping setup
//! cost out of the measurement.

// External crates
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use pill_engine::{Component, Entity, Query, World};
use trait_type_map::impl_trait_accessible;

// =============================================================================
// Constants
// =============================================================================

/// Number of entities populated for the query iteration benchmarks.
const QUERY_ENTITIES: usize = 100_000;

// =============================================================================
// Position
// =============================================================================

/// Two-dimensional position component used by the query benchmarks.
///
/// Larger than a scalar component, so reading it stresses the iterator's
/// memory-bandwidth path.
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

/// Two-dimensional velocity component used by the query benchmarks.
///
/// Read alongside [`Position`] so unfiltered queries iterate two components
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
// Trait Accessibility
// =============================================================================

// Registers the component types as trait-accessible so the type-erased
// query machinery can downcast to them at runtime.
impl_trait_accessible!(dyn Component; Position, Velocity);

// =============================================================================
// Free Functions
// =============================================================================

/// Builds a fresh [`World`](pill_engine::World) pre-populated with
/// `entity_count` entities.
///
/// Every entity carries [`Position`] and [`Velocity`] components so all
/// entities match the unfiltered query archetype and setup cost stays uniform
/// across benchmarks.
///
/// Returns the world together with the handles of the created entities.
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
fn query_iter_unfiltered(criterion: &mut Criterion) {
    let (mut world, _) = setup_world(QUERY_ENTITIES);
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

criterion_group!(benches, query_iter_unfiltered);
criterion_main!(benches);
