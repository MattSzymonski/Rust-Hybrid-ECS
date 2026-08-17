//! Minimal benchmarks for the main query and archetype-migration hot paths.
//!
//! # Responsibilities
//!
//! - Benchmarks sequential unfiltered and change-filtered query iteration.
//! - Benchmarks parallel (Rayon) query iteration.
//! - Benchmarks archetype migration triggered by [`World::add_component`].
//! - Defines the [`Position`], [`Velocity`], and [`Health`] component types
//!   shared by every benchmark.
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
use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use pill_engine::{Changed, Component, Entity, Query, World};
use trait_type_map::impl_trait_accessible;

// =============================================================================
// Constants
// =============================================================================

/// Number of entities populated for the query iteration benchmarks.
const QUERY_ENTITIES: usize = 100_000;

/// Number of entities populated for the archetype-migration benchmark.
const MIGRATION_ENTITIES: usize = 10_000;

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
// Health
// =============================================================================

/// Single-field health component used by the archetype-migration benchmark.
///
/// Added to entities after creation to force an archetype migration.
#[derive(Clone)]
struct Health(f32);
impl Component for Health {}

// =============================================================================
// Trait Accessibility
// =============================================================================

// Registers the three component types as trait-accessible so the type-erased
// query machinery can downcast to them at runtime.
impl_trait_accessible!(dyn Component; Position, Velocity, Health);

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
    world.register_component::<Health>();

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

/// Benchmarks sequential iteration with a [`Changed`] filter.
///
/// Every entity is mutated and the world tick is bumped before measuring so
/// the `Changed` filter matches the full entity set.
fn query_iter_changed(criterion: &mut Criterion) {
    let (mut world, _) = setup_world(QUERY_ENTITIES);
    // Step 1: Mutate every Position so all entities appear changed; the block
    // scope releases the mutable query borrow before the tick is bumped.
    {
        let mut query = Query::<&mut Position>::new(&mut world);
        for mut position in query.iter_mut() {
            position.x += 1.0;
        }
    }
    // Step 2: Bump the world tick so the mutations form the newest change set.
    world.increment_change_tick();

    criterion.bench_function("query_iter_changed", |benchmark| {
        benchmark.iter(|| {
            let mut matched = 0;
            let mut query = Query::<(&Position,), Changed<Position>>::new(&mut world);
            for (position,) in query.iter_mut() {
                black_box(position.x);
                matched += 1;
            }
            black_box(matched);
        });
    });
}

/// Benchmarks parallel (Rayon) unfiltered iteration over every entity.
///
/// Work is distributed across the Rayon thread pool; the per-entity result is
/// passed through [`black_box`] to prevent dead-code elimination.
fn query_par_iter_unfiltered(criterion: &mut Criterion) {
    let (mut world, _) = setup_world(QUERY_ENTITIES);
    criterion.bench_function("query_par_iter_unfiltered", |benchmark| {
        benchmark.iter(|| {
            let mut query = Query::<(&Position, &Velocity)>::new(&mut world);
            query.par_iter_mut().for_each(|(position, velocity)| {
                black_box(position.x + position.y + velocity.x + velocity.y);
            });
        });
    });
}

/// Benchmarks archetype migration caused by adding a component at runtime.
///
/// Uses `iter_batched` with [`BatchSize::LargeInput`] so each iteration
/// rebuilds the world in the setup closure, keeping setup cost out of the
/// measurement.
fn archetype_add_component(criterion: &mut Criterion) {
    criterion.bench_function("archetype_add_component", |benchmark| {
        benchmark.iter_batched(
            || setup_world(MIGRATION_ENTITIES),
            |(mut world, entities)| {
                for entity in entities {
                    // Adding Health forces an archetype migration for that entity.
                    black_box(
                        world
                            .add_component(entity, Health(entity.id() as f32))
                            .is_ok(),
                    );
                }
            },
            BatchSize::LargeInput,
        );
    });
}

// =============================================================================
// Benchmark Registration
// =============================================================================

criterion_group!(
    benches,
    query_iter_unfiltered,
    // query_iter_changed,
    // query_par_iter_unfiltered,
    // archetype_add_component,
);
criterion_main!(benches);
