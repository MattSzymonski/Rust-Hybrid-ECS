//! Unit tests for the query system - covers basic queries, parallel
//! iteration, change detection, and filters.

use super::*;
use crate::component::{Component, ComponentId};
use crate::entity::Entity;
use crate::world::World;
use trait_type_map::impl_trait_accessible;

// Test components
#[derive(Debug, Clone, PartialEq)]
struct Position {
    x: f32,
    y: f32,
}
impl Component for Position {}

#[derive(Debug, Clone, PartialEq)]
struct Velocity {
    x: f32,
    y: f32,
}
impl Component for Velocity {}

#[derive(Debug, Clone, PartialEq)]
struct Health(i32);
impl Component for Health {}

impl_trait_accessible!(dyn Component; Position, Velocity, Health);

fn setup_world() -> World {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();
    world
}

// ------------------------------------------------------------------------
// Basic Query Tests
// ------------------------------------------------------------------------

#[test]
fn test_query_empty_world() {
    let mut world = setup_world();
    let mut query = Query::<(&Position,)>::new(&mut world);
    assert_eq!(query.iter_mut().count(), 0);
}

#[test]
fn test_query_single_entity() {
    let mut world = setup_world();
    world
        .create_entity()
        .with(Position { x: 1.0, y: 2.0 })
        .with(Velocity { x: 0.5, y: 0.5 })
        .build();

    let mut query = Query::<(&Position, &Velocity)>::new(&mut world);
    let results: Vec<_> = query.iter_mut().collect();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0.x, 1.0);
    assert_eq!(results[0].1.x, 0.5);
}

#[test]
fn test_query_multiple_entities() {
    let mut world = setup_world();

    for i in 0..10 {
        world
            .create_entity()
            .with(Position {
                x: i as f32,
                y: 0.0,
            })
            .build();
    }

    let mut query = Query::<(&Position,)>::new(&mut world);
    assert_eq!(query.iter_mut().count(), 10);
}

#[test]
fn test_query_filters_by_components() {
    let mut world = setup_world();

    // Entity with Position only
    world
        .create_entity()
        .with(Position { x: 1.0, y: 1.0 })
        .build();

    // Entity with Position and Velocity
    world
        .create_entity()
        .with(Position { x: 2.0, y: 2.0 })
        .with(Velocity { x: 1.0, y: 1.0 })
        .build();

    // Query for entities with both Position and Velocity
    let mut query = Query::<(&Position, &Velocity)>::new(&mut world);
    let results: Vec<_> = query.iter_mut().collect();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0.x, 2.0);
}

#[test]
fn test_query_mutable_modification() {
    let mut world = setup_world();

    world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .with(Velocity { x: 1.0, y: 2.0 })
        .build();

    // Modify position based on velocity
    {
        let mut query = Query::<(&mut Position, &Velocity)>::new(&mut world);
        for (mut pos, vel) in query.iter_mut() {
            pos.x += vel.x;
            pos.y += vel.y;
        }
    }

    // Verify modification
    let mut query = Query::<(&Position,)>::new(&mut world);
    let (pos,) = query.first().unwrap();
    assert_eq!(pos.x, 1.0);
    assert_eq!(pos.y, 2.0);
}

#[test]
fn test_query_first() {
    let mut world = setup_world();

    world
        .create_entity()
        .with(Position { x: 5.0, y: 5.0 })
        .build();

    let mut query = Query::<(&Position,)>::new(&mut world);
    let first = query.first();

    assert!(first.is_some());
    assert_eq!(first.unwrap().0.x, 5.0);
}

#[test]
fn test_query_first_empty() {
    let mut world = setup_world();
    let mut query = Query::<(&Position,)>::new(&mut world);
    assert!(query.first().is_none());
}

#[test]
fn test_query_entity_access() {
    let mut world = setup_world();

    let entity = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build();

    let mut query = Query::<(Entity, &Position)>::new(&mut world);
    let (queried_entity, _) = query.first().unwrap();

    assert_eq!(queried_entity.id(), entity.id());
}

// ------------------------------------------------------------------------
// Parallel Iterator Tests
// ------------------------------------------------------------------------

#[test]
fn test_par_iter_basic() {
    let mut world = setup_world();

    for i in 0..100 {
        world
            .create_entity()
            .with(Position {
                x: i as f32,
                y: 0.0,
            })
            .build();
    }

    let mut query = Query::<(&mut Position,)>::new(&mut world);
    query.par_iter_mut().for_each(|(mut pos,)| {
        pos.x += 1.0;
    });

    // Verify all were modified
    let mut verify_query = Query::<(&Position,)>::new(&mut world);
    for (pos,) in verify_query.iter_mut() {
        assert!(pos.x >= 1.0);
    }
}

#[test]
fn test_par_iter_with_batch_size() {
    let mut world = setup_world();

    for _ in 0..1000 {
        world
            .create_entity()
            .with(Position { x: 0.0, y: 0.0 })
            .build();
    }

    let mut query = Query::<(&mut Position,)>::new(&mut world);
    let stats = query
        .par_iter_mut()
        .with_batch_size(100)
        .tracked()
        .for_each(|(mut pos,)| {
            pos.x = 1.0;
        });

    let stats = stats.unwrap();
    assert_eq!(stats.total_entities, 1000);
    assert!(stats.batch_count > 0);
    assert!(stats.min_batch_size >= 100 || stats.batch_count == 1);
}

#[test]
fn test_par_iter_tracked_stats() {
    let mut world = setup_world();

    for _ in 0..500 {
        world
            .create_entity()
            .with(Position { x: 0.0, y: 0.0 })
            .build();
    }

    let mut query = Query::<(&Position,)>::new(&mut world);
    let result = query.par_iter_mut().tracked().for_each(|_| {});

    match result {
        ParForEachResult::Tracked(stats) => {
            assert_eq!(stats.total_entities, 500);
            assert!(stats.batch_count > 0);
            assert!(stats.num_threads > 0);
            assert!(stats.avg_batch_size > 0.0);
        }
        ParForEachResult::Untracked => panic!("Expected tracked result"),
    }
}

#[test]
fn test_par_iter_untracked() {
    let mut world = setup_world();

    world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build();

    let mut query = Query::<(&Position,)>::new(&mut world);
    let result = query.par_iter_mut().for_each(|_| {});

    assert!(matches!(result, ParForEachResult::Untracked));
    assert!(result.stats().is_none());
}

#[test]
fn test_par_iter_entity_count() {
    let mut world = setup_world();

    for _ in 0..250 {
        world
            .create_entity()
            .with(Position { x: 0.0, y: 0.0 })
            .build();
    }

    let mut query = Query::<(&Position,)>::new(&mut world);
    let par_iter = query.par_iter_mut();

    assert_eq!(par_iter.entity_count(), 250);
}

// ------------------------------------------------------------------------
// BatchStats Tests
// ------------------------------------------------------------------------

#[test]
fn test_batch_stats_display() {
    let stats = BatchStats {
        num_threads: 8,
        batch_count: 10,
        total_entities: 1000,
        min_batch_size: 90,
        max_batch_size: 110,
        avg_batch_size: 100.0,
    };

    let display = format!("{}", stats);
    assert!(display.contains("threads: 8"));
    assert!(display.contains("batches: 10"));
    assert!(display.contains("entities: 1000"));
}

#[test]
fn test_par_for_each_result_display() {
    let stats = BatchStats {
        num_threads: 4,
        batch_count: 5,
        total_entities: 100,
        min_batch_size: 20,
        max_batch_size: 20,
        avg_batch_size: 20.0,
    };

    let tracked = ParForEachResult::Tracked(stats);
    let untracked = ParForEachResult::Untracked;

    assert!(format!("{}", tracked).contains("threads: 4"));
    assert_eq!(format!("{}", untracked), "Untracked");
}

// ------------------------------------------------------------------------
// QueryTarget Trait Tests
// ------------------------------------------------------------------------

#[test]
fn test_component_ids() {
    let ids = <(&Position, &Velocity)>::component_ids();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&ComponentId::of::<Position>()));
    assert!(ids.contains(&ComponentId::of::<Velocity>()));
}

#[test]
fn test_report_component_access_read() {
    let (reads, writes) = <(&Position,)>::report_component_access();
    assert_eq!(reads.len(), 1);
    assert_eq!(writes.len(), 0);
}

#[test]
fn test_report_component_access_write() {
    let (reads, writes) = <(&mut Position,)>::report_component_access();
    assert_eq!(reads.len(), 0);
    assert_eq!(writes.len(), 1);
}

#[test]
fn test_report_component_access_mixed() {
    let (reads, writes) = <(&Position, &mut Velocity)>::report_component_access();
    assert_eq!(reads.len(), 1);
    assert_eq!(writes.len(), 1);
}

#[test]
fn test_entity_has_no_component_ids() {
    let ids = Entity::component_ids();
    assert!(ids.is_empty());
}

// ------------------------------------------------------------------------
// Change Detection Tests
// ------------------------------------------------------------------------

#[test]
fn test_mut_deref_bumps_changed_tick() {
    let mut world = setup_world();
    world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build();

    // Capture the tick at which the component was added (insert
    // happened with whatever world.change_tick was at the time).
    let added_tick = {
        let mut q = Query::<(&mut Position,)>::new(&mut world);
        let (m,) = q.first().unwrap();
        m.last_added()
    };

    // Mutate via DerefMut - should bump `changed` to the iterator's
    // this_run, which is strictly newer than `added`.
    let changed_tick = {
        let mut q = Query::<(&mut Position,)>::new(&mut world);
        let (mut m,) = q.first().unwrap();
        m.x += 5.0; // DerefMut path
        m.last_changed()
    };

    assert!(
        changed_tick > added_tick,
        "expected changed ({:?}) > added ({:?})",
        changed_tick,
        added_tick
    );
}

#[test]
fn test_immutable_deref_does_not_bump_changed_tick() {
    let mut world = setup_world();
    world
        .create_entity()
        .with(Position { x: 1.0, y: 2.0 })
        .build();

    let baseline = {
        let mut q = Query::<(&mut Position,)>::new(&mut world);
        let (m,) = q.first().unwrap();
        m.last_changed()
    };

    // Read-only deref must not advance the changed tick.
    let after_read = {
        let mut q = Query::<(&mut Position,)>::new(&mut world);
        let (m,) = q.first().unwrap();
        let _x = m.x; // immutable deref
        m.last_changed()
    };

    assert_eq!(baseline, after_read);
}

#[test]
fn test_bypass_change_detection_does_not_bump_tick() {
    let mut world = setup_world();
    world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build();

    let baseline = {
        let mut q = Query::<(&mut Position,)>::new(&mut world);
        let (m,) = q.first().unwrap();
        m.last_changed()
    };

    let after_bypass = {
        let mut q = Query::<(&mut Position,)>::new(&mut world);
        let (mut m,) = q.first().unwrap();
        m.bypass_change_detection().x = 99.0;
        m.last_changed()
    };

    assert_eq!(baseline, after_bypass);

    // Verify the actual mutation still happened.
    let mut verify = Query::<(&Position,)>::new(&mut world);
    let (pos,) = verify.first().unwrap();
    assert_eq!(pos.x, 99.0);
}

#[test]
fn test_added_tick_preserved_across_archetype_migration() {
    let mut world = setup_world();
    let entity = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build();

    let original_added = {
        let mut q = Query::<(&mut Position,)>::new(&mut world);
        let (m,) = q.first().unwrap();
        m.last_added()
    };

    // Migrate to a new archetype by adding a component. The Position
    // component carries over and its `added` tick must be preserved.
    world
        .add_component(entity, Velocity { x: 1.0, y: 1.0 })
        .unwrap();

    let after_migration = {
        let mut q = Query::<(&mut Position,)>::new(&mut world);
        let (m,) = q.first().unwrap();
        m.last_added()
    };

    assert_eq!(original_added, after_migration);

    // The newly attached Velocity should have an `added` tick that is
    // at least as new as the original Position's added tick.
    let velocity_added = {
        let mut q = Query::<(&mut Velocity,)>::new(&mut world);
        let (m,) = q.first().unwrap();
        m.last_added()
    };
    assert!(velocity_added >= original_added);
}

// ------------------------------------------------------------------------
// QueryFilter Tests
// ------------------------------------------------------------------------

#[test]
fn test_filter_with_includes_only_matching_archetypes() {
    let mut world = setup_world();
    // Two entities: one with Health, one without.
    world
        .create_entity()
        .with(Position { x: 1.0, y: 0.0 })
        .with(Health(100))
        .build();
    world
        .create_entity()
        .with(Position { x: 2.0, y: 0.0 })
        .build();

    let mut q = Query::<(&Position,), With<Health>>::new(&mut world);
    let xs: Vec<f32> = q.iter_mut().map(|(p,)| p.x).collect();
    assert_eq!(xs, vec![1.0]);
}

#[test]
fn test_filter_without_excludes_archetypes() {
    let mut world = setup_world();
    world
        .create_entity()
        .with(Position { x: 1.0, y: 0.0 })
        .with(Health(100))
        .build();
    world
        .create_entity()
        .with(Position { x: 2.0, y: 0.0 })
        .build();

    let mut q = Query::<(&Position,), Without<Health>>::new(&mut world);
    let xs: Vec<f32> = q.iter_mut().map(|(p,)| p.x).collect();
    assert_eq!(xs, vec![2.0]);
}

#[test]
fn test_filter_changed_detects_mutated_rows_only() {
    let mut world = setup_world();
    let e1 = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build();
    let _e2 = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build();
    let _e3 = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build();

    // Set the baseline so all "Added" ticks are in the past.
    world.set_system_last_run(world.change_tick());

    // Mutate only e1.
    {
        let mut q = Query::<(Entity, &mut Position)>::new(&mut world);
        for (entity, mut pos) in q.iter_mut() {
            if entity == e1 {
                pos.x = 99.0;
            }
        }
    }

    // Filter on Changed<Position>: only e1 should appear.
    let mut q = Query::<(Entity, &Position), Changed<Position>>::new(&mut world);
    let hits: Vec<Entity> = q.iter_mut().map(|(e, _)| e).collect();
    assert_eq!(hits, vec![e1]);
}

#[test]
fn test_filter_added_detects_newly_inserted_components() {
    let mut world = setup_world();
    let _old = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build();

    // Lock in the current tick as the baseline so the existing entity
    // is NOT considered "added" relative to it.
    world.set_system_last_run(world.change_tick());

    // Bump the tick (simulating a frame boundary) and insert a new
    // entity - it should be the only `Added<Position>` hit.
    world.increment_change_tick();
    let new_entity = world
        .create_entity()
        .with(Position { x: 1.0, y: 1.0 })
        .build();

    let mut q = Query::<(Entity,), Added<Position>>::new(&mut world);
    let hits: Vec<Entity> = q.iter_mut().map(|(e,)| e).collect();
    assert_eq!(hits, vec![new_entity]);
}

#[test]
fn test_filter_or_combines_predicates() {
    let mut world = setup_world();
    let e1 = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .with(Velocity { x: 0.0, y: 0.0 })
        .build();
    let e2 = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .with(Velocity { x: 0.0, y: 0.0 })
        .build();

    world.set_system_last_run(world.change_tick());

    // Mutate Position on e1, Velocity on e2.
    {
        let mut q = Query::<(Entity, &mut Position, &mut Velocity)>::new(&mut world);
        for (entity, mut pos, mut vel) in q.iter_mut() {
            if entity == e1 {
                pos.x = 1.0;
            } else if entity == e2 {
                vel.x = 1.0;
            }
        }
    }

    let mut q =
        Query::<(Entity,), Or<(Changed<Position>, Changed<Velocity>)>>::new(&mut world);
    let mut hits: Vec<Entity> = q.iter_mut().map(|(e,)| e).collect();
    hits.sort_by_key(|e| e.id());
    let mut expected = vec![e1, e2];
    expected.sort_by_key(|e| e.id());
    assert_eq!(hits, expected);
}

#[test]
fn test_filter_changed_empty_after_no_mutation() {
    let mut world = setup_world();
    world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build();

    // Baseline at the current tick - the existing component's
    // `changed` tick is older, so the filter must yield nothing.
    world.set_system_last_run(world.change_tick());

    let mut q = Query::<(Entity,), Changed<Position>>::new(&mut world);
    assert_eq!(q.iter_mut().count(), 0);
}
