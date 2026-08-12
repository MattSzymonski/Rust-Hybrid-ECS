//! Minimal benchmarks for the main query and archetype-migration hot paths.

use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use pill_engine::{Changed, Component, Entity, Query, World};
use trait_type_map::impl_trait_accessible;

const QUERY_ENTITIES: usize = 100_000;
const MIGRATION_ENTITIES: usize = 10_000;

#[derive(Clone)]
struct Position {
    x: f32,
    y: f32,
}
impl Component for Position {}

#[derive(Clone)]
struct Velocity {
    x: f32,
    y: f32,
}
impl Component for Velocity {}

#[derive(Clone)]
struct Health(f32);
impl Component for Health {}

impl_trait_accessible!(dyn Component; Position, Velocity, Health);

fn setup_world(entity_count: usize) -> (World, Vec<Entity>) {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();

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

fn query_iter_changed(criterion: &mut Criterion) {
    let (mut world, _) = setup_world(QUERY_ENTITIES);
    {
        let mut query = Query::<&mut Position>::new(&mut world);
        for mut position in query.iter_mut() {
            position.x += 1.0;
        }
    }
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

fn archetype_add_component(criterion: &mut Criterion) {
    criterion.bench_function("archetype_add_component", |benchmark| {
        benchmark.iter_batched(
            || setup_world(MIGRATION_ENTITIES),
            |(mut world, entities)| {
                for entity in entities {
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

criterion_group!(
    benches,
    query_iter_unfiltered,
   // query_iter_changed,
    // query_par_iter_unfiltered,
    // archetype_add_component,
);
criterion_main!(benches);
