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
        .build()
        .unwrap();

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
            .build()
            .unwrap();
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
        .build()
        .unwrap();

    // Entity with Position and Velocity
    world
        .create_entity()
        .with(Position { x: 2.0, y: 2.0 })
        .with(Velocity { x: 1.0, y: 1.0 })
        .build()
        .unwrap();

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
        .build()
        .unwrap();

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
        .build()
        .unwrap();

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
        .build()
        .unwrap();

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
            .build()
            .unwrap();
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
            .build()
            .unwrap();
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
            .build()
            .unwrap();
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
        .build()
        .unwrap();

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
            .build()
            .unwrap();
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
#[cfg(debug_assertions)]
#[should_panic(expected = "duplicate mutable component types")]
fn test_duplicate_mutable_types_rejected() {
    // Query<(&mut Position, &mut Position)> would create aliasing &mut
    // references to the same storage - UB. The debug_assert! inside
    // report_component_access() must catch this.
    let _ = <(&mut Position, &mut Position)>::report_component_access();
}

#[test]
fn test_has_duplicate_writes_detection() {
    use crate::query::target::has_duplicate_writes;
    let id = ComponentId::of::<Position>();
    assert!(!has_duplicate_writes(&[]));
    assert!(!has_duplicate_writes(&[id]));
    assert!(!has_duplicate_writes(&[id, ComponentId::of::<Velocity>()]));
    assert!(has_duplicate_writes(&[id, id]));
    assert!(has_duplicate_writes(&[
        id,
        ComponentId::of::<Velocity>(),
        id
    ]));
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
        .build()
        .unwrap();

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
        .build()
        .unwrap();

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
        .build()
        .unwrap();

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
        .build()
        .unwrap();

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
        .build()
        .unwrap();
    world
        .create_entity()
        .with(Position { x: 2.0, y: 0.0 })
        .build()
        .unwrap();

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
        .build()
        .unwrap();
    world
        .create_entity()
        .with(Position { x: 2.0, y: 0.0 })
        .build()
        .unwrap();

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
        .build()
        .unwrap();
    let _e2 = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();
    let _e3 = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();

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
        .build()
        .unwrap();

    // Lock in the current tick as the baseline so the existing entity
    // is NOT considered "added" relative to it.
    world.set_system_last_run(world.change_tick());

    // Bump the tick (simulating a frame boundary) and insert a new
    // entity - it should be the only `Added<Position>` hit.
    world.increment_change_tick();
    let new_entity = world
        .create_entity()
        .with(Position { x: 1.0, y: 1.0 })
        .build()
        .unwrap();

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
        .build()
        .unwrap();
    let e2 = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .with(Velocity { x: 0.0, y: 0.0 })
        .build()
        .unwrap();

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

    let mut q = Query::<(Entity,), Or<(Changed<Position>, Changed<Velocity>)>>::new(&mut world);
    let mut hits: Vec<Entity> = q.iter_mut().map(|(e,)| e).collect();
    hits.sort_by_key(|e| e.id());
    let mut expected = vec![e1, e2];
    expected.sort_by_key(|e| e.id());
    assert_eq!(hits, expected);
}

#[test]
fn test_filter_or_with_components_either() {
    // Regression test for issue 4.8: Or<(With<A>, With<B>)> must match
    // archetypes containing A *or* B (not just both). The old
    // implementation incorrectly required both A and B because
    // included_component_ids was built via union (intersection semantics).
    let mut world = setup_world();
    let e_a = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .with(Health(10))
        .build()
        .unwrap();
    let e_b = world
        .create_entity()
        .with(Velocity { x: 0.0, y: 0.0 })
        .with(Health(10))
        .build()
        .unwrap();
    let e_both = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .with(Velocity { x: 0.0, y: 0.0 })
        .with(Health(10))
        .build()
        .unwrap();
    // This entity has neither A nor B - should be excluded.
    let _e_none = world.create_entity().with(Health(10)).build().unwrap();

    // Query for entities that have Position OR Velocity (via With filter).
    let mut q = Query::<(Entity,), Or<(With<Position>, With<Velocity>)>>::new(&mut world);
    let mut hits: Vec<Entity> = q.iter_mut().map(|(e,)| e).collect();
    hits.sort_by_key(|e| e.id());

    let mut expected = vec![e_a, e_b, e_both];
    expected.sort_by_key(|e| e.id());
    assert_eq!(
        hits, expected,
        "Or<(With<A>, With<B>)> should match A-only, B-only, and both"
    );
}

#[test]
fn test_filter_or_with_without_correct_archetypes() {
    // Or<(With<A>, Without<B>)>: match archetypes containing A, *or*
    // archetypes not containing B. An archetype with neither A nor B
    // should match via the Without<B> branch.
    let mut world = setup_world();
    let e_a = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .with(Velocity { x: 0.0, y: 0.0 })
        .build()
        .unwrap();
    // e_b has Velocity but no Position - excluded by both branches.
    let _e_b = world
        .create_entity()
        .with(Velocity { x: 0.0, y: 0.0 })
        .with(Health(10))
        .build()
        .unwrap();
    let e_none = world.create_entity().with(Health(10)).build().unwrap();

    let mut q = Query::<(Entity,), Or<(With<Position>, Without<Velocity>)>>::new(&mut world);
    let mut hits: Vec<Entity> = q.iter_mut().map(|(e,)| e).collect();
    hits.sort_by_key(|e| e.id());

    // e_a has Position → matches via With<Position>
    // e_b has Velocity → excluded by Without<Velocity> BUT has no Position
    //   → does NOT match either branch → excluded
    // e_none has neither Position nor Velocity → matches via Without<Velocity>
    let mut expected = vec![e_a, e_none];
    expected.sort_by_key(|e| e.id());
    assert_eq!(hits, expected);
}

#[test]
fn test_filter_or_mixed_with_and_changed() {
    // Or<(With<A>, Changed<B>)>: mixed archetype-level (With) and
    // row-level (Changed) filters. An archetype with A but not B must
    // NOT panic in Changed<B>::init_state.
    let mut world = setup_world();
    let e_a = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .with(Health(10))
        .build()
        .unwrap();
    let e_b = world
        .create_entity()
        .with(Velocity { x: 0.0, y: 0.0 })
        .with(Health(10))
        .build()
        .unwrap();
    let e_both = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .with(Velocity { x: 0.0, y: 0.0 })
        .with(Health(10))
        .build()
        .unwrap();

    world.set_system_last_run(world.change_tick());

    // Mutate Position on e_both, Velocity on e_b.
    {
        let mut q = Query::<(Entity, &mut Position, &mut Velocity)>::new(&mut world);
        for (entity, mut pos, mut vel) in q.iter_mut() {
            if entity == e_both {
                pos.x = 1.0;
            } else if entity == e_b {
                vel.x = 1.0;
            }
        }
    }

    // e_a: has Position (With<Position> matches) → included by branch 1.
    //      Changed<Velocity> init_state must not panic (Velocity absent).
    // e_b: has Velocity, and Velocity was changed → matches branch 2.
    // e_both: has Position → matches branch 1 (also has Velocity changed
    //         but short-circuits before checking).
    let mut q = Query::<(Entity,), Or<(With<Position>, Changed<Velocity>)>>::new(&mut world);
    let mut hits: Vec<Entity> = q.iter_mut().map(|(e,)| e).collect();
    hits.sort_by_key(|e| e.id());

    let mut expected = vec![e_a, e_b, e_both];
    expected.sort_by_key(|e| e.id());
    assert_eq!(hits, expected);
}

#[test]
fn test_filter_or_without_or_without() {
    // Or<(Without<A>, Without<B>)>: include archetypes that lack A *or*
    // lack B. Only archetypes containing BOTH A and B are excluded.
    let mut world = setup_world();
    let e_a = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .with(Health(10))
        .build()
        .unwrap();
    let e_b = world
        .create_entity()
        .with(Velocity { x: 0.0, y: 0.0 })
        .with(Health(10))
        .build()
        .unwrap();
    // e_both has both Position and Velocity → excluded by both Without branches
    let _e_both = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .with(Velocity { x: 0.0, y: 0.0 })
        .build()
        .unwrap();
    let e_none = world.create_entity().with(Health(10)).build().unwrap();

    let mut q = Query::<(Entity,), Or<(Without<Position>, Without<Velocity>)>>::new(&mut world);
    let mut hits: Vec<Entity> = q.iter_mut().map(|(e,)| e).collect();
    hits.sort_by_key(|e| e.id());

    // e_a: lacks Velocity → matches Without<Velocity>
    // e_b: lacks Position → matches Without<Position>
    // e_both: has both → excluded
    // e_none: lacks both → matches both branches
    let mut expected = vec![e_a, e_b, e_none];
    expected.sort_by_key(|e| e.id());
    assert_eq!(
        hits, expected,
        "Or<(Without<A>, Without<B>)> should exclude only (A+B) archetypes"
    );
}

#[test]
fn test_filter_or_three_way() {
    // Or<(With<A>, With<B>, With<C>)>: 3-way OR at archetype level.
    let mut world = setup_world();
    let e_pos = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();
    let e_vel = world
        .create_entity()
        .with(Velocity { x: 0.0, y: 0.0 })
        .build()
        .unwrap();
    let e_hp = world.create_entity().with(Health(10)).build().unwrap();
    // Entity with none of the three (empty archetype isn't possible, but
    // we can make one with a different component - no, we don't have a
    // 4th component. That's fine - the test covers A, B, C archetypes.)

    let mut q =
        Query::<(Entity,), Or<(With<Position>, With<Velocity>, With<Health>)>>::new(&mut world);
    let mut hits: Vec<Entity> = q.iter_mut().map(|(e,)| e).collect();
    hits.sort_by_key(|e| e.id());

    assert_eq!(
        hits.len(),
        3,
        "all three entities should match via different branches"
    );
    // e_pos matches via With<Position>, e_vel via With<Velocity>,
    // e_hp via With<Health>.
    let ids: Vec<u64> = hits.iter().map(|e| e.id()).collect();
    assert!(ids.contains(&e_pos.id()));
    assert!(ids.contains(&e_vel.id()));
    assert!(ids.contains(&e_hp.id()));
}

#[test]
fn test_filter_or_within_and_tuple() {
    // (Or<(With<A>, With<B>)>, Without<C>): Or inside an AND tuple.
    // Match archetypes that (have A or B) AND do NOT have C.
    let mut world = setup_world();
    // Archetype: Position + Health  (has A, no B, has C=Health → excluded by Without<Health>)
    let _e_a_c = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .with(Health(10))
        .build()
        .unwrap();
    // Archetype: Position only (has A, no B, no C) → matches
    let e_a = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();
    // Archetype: Velocity only (no A, has B, no C) → matches
    let e_b = world
        .create_entity()
        .with(Velocity { x: 0.0, y: 0.0 })
        .build()
        .unwrap();
    // Archetype: Health only (no A, no B, has C) → OR branch fails, Without fails
    let _e_c = world.create_entity().with(Health(10)).build().unwrap();

    let mut q = Query::<(Entity,), (Or<(With<Position>, With<Velocity>)>, Without<Health>)>::new(
        &mut world,
    );
    let mut hits: Vec<Entity> = q.iter_mut().map(|(e,)| e).collect();
    hits.sort_by_key(|e| e.id());

    let mut expected = vec![e_a, e_b];
    expected.sort_by_key(|e| e.id());
    assert_eq!(hits, expected);
}

#[test]
fn test_filter_or_empty_target_with_or() {
    // Query<(), Or<(With<A>, With<B>)>>: no data fetched, only filter.
    // Should still correctly scope to matching archetypes.
    let mut world = setup_world();
    let e_a = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();
    let e_b = world
        .create_entity()
        .with(Velocity { x: 0.0, y: 0.0 })
        .build()
        .unwrap();
    let _e_none = world.create_entity().with(Health(10)).build().unwrap();

    // () as QueryTarget - fetches no data, but Entity is available via iter_mut
    let mut q = Query::<(Entity,), Or<(With<Position>, With<Velocity>)>>::new(&mut world);
    let mut hits: Vec<Entity> = q.iter_mut().map(|(e,)| e).collect();
    hits.sort_by_key(|e| e.id());

    let mut expected = vec![e_a, e_b];
    expected.sort_by_key(|e| e.id());
    assert_eq!(hits, expected);
}

#[test]
fn test_filter_or_no_double_count() {
    // An entity in an archetype that matches multiple Or branches must
    // appear exactly once, not once per matching branch.
    let mut world = setup_world();
    let e_both = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .with(Velocity { x: 0.0, y: 0.0 })
        .build()
        .unwrap();

    let mut q = Query::<(Entity,), Or<(With<Position>, With<Velocity>)>>::new(&mut world);
    let hits: Vec<Entity> = q.iter_mut().map(|(e,)| e).collect();
    assert_eq!(
        hits.len(),
        1,
        "entity should appear once even if both branches match"
    );
    assert_eq!(hits[0], e_both);
}

#[test]
fn test_filter_or_par_iter() {
    // Parallel iteration with Or filter - smoke test for correctness.
    // Uses for_each with a Mutex since ParQueryIter doesn't implement
    // rayon::ParallelIterator directly.
    use std::sync::Mutex;

    let mut world = setup_world();
    for i in 0..50 {
        if i % 2 == 0 {
            world
                .create_entity()
                .with(Position {
                    x: i as f32,
                    y: 0.0,
                })
                .build()
                .unwrap();
        } else {
            world
                .create_entity()
                .with(Velocity {
                    x: i as f32,
                    y: 0.0,
                })
                .build()
                .unwrap();
        }
    }

    let mut q = Query::<(Entity,), Or<(With<Position>, With<Velocity>)>>::new(&mut world);
    let hits = Mutex::new(Vec::new());
    q.par_iter_mut().for_each(|(e,)| {
        hits.lock().unwrap().push(e);
    });
    let hits = hits.into_inner().unwrap();
    assert_eq!(
        hits.len(),
        50,
        "Or filter with par_iter should find all entities"
    );
}

#[test]
fn test_filter_or_with_unit_branch_always_true() {
    // Or<(With<A>, ())>: the () branch has no restrictions and always
    // matches, so the whole Or should match EVERY entity regardless of
    // whether it has A or not.
    let mut world = setup_world();
    let e_a = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();
    let e_no_a = world.create_entity().with(Health(10)).build().unwrap();
    let e_vel = world
        .create_entity()
        .with(Velocity { x: 0.0, y: 0.0 })
        .build()
        .unwrap();

    let mut q = Query::<(Entity,), Or<(With<Position>, ())>>::new(&mut world);
    let mut hits: Vec<Entity> = q.iter_mut().map(|(e,)| e).collect();
    hits.sort_by_key(|e| e.id());

    // All entities should match because the () branch has no restrictions.
    let mut expected = vec![e_a, e_no_a, e_vel];
    expected.sort_by_key(|e| e.id());
    assert_eq!(hits, expected, "Or<(... , ())> should match all entities");
}

#[test]
fn test_filter_or_with_unit_branch_first() {
    // Or<((), With<A>)>: order of branches shouldn't matter. The ()
    // branch (first position) still makes the whole Or match everything.
    let mut world = setup_world();
    let e_any = world.create_entity().with(Health(10)).build().unwrap();

    let mut q = Query::<(Entity,), Or<((), With<Position>)>>::new(&mut world);
    let hits: Vec<Entity> = q.iter_mut().map(|(e,)| e).collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0], e_any);
}

#[test]
fn test_filter_or_duplicate_inner_filters() {
    // Or<(With<A>, With<A>)>: redundant inner filters - should behave
    // identically to a single With<A> (no double-counting, no panic).
    let mut world = setup_world();
    let e_a = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();
    let _e_no_a = world.create_entity().with(Health(10)).build().unwrap();

    let mut q = Query::<(Entity,), Or<(With<Position>, With<Position>)>>::new(&mut world);
    let hits: Vec<Entity> = q.iter_mut().map(|(e,)| e).collect();
    assert_eq!(
        hits.len(),
        1,
        "duplicate inner filters should not double-count"
    );
    assert_eq!(hits[0], e_a);
}

#[test]
fn test_filter_and_of_two_ors() {
    // (Or<(With<A>, With<B>)>, Or<(Without<A>, Without<B>)>)
    // Cross-product: (A or B) AND (not-A or not-B)
    // = (A and not-B) or (B and not-A)
    // Entities with only A match; entities with only B match;
    // entities with both A and B do NOT match (A fails Without<A>,
    // B fails Without<B>, neither branch of second Or passes).
    let mut world = setup_world();
    let e_a = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .with(Health(10))
        .build()
        .unwrap();
    let e_b = world
        .create_entity()
        .with(Velocity { x: 0.0, y: 0.0 })
        .with(Health(10))
        .build()
        .unwrap();
    let _e_both = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .with(Velocity { x: 0.0, y: 0.0 })
        .build()
        .unwrap();

    type F = (
        Or<(With<Position>, With<Velocity>)>,
        Or<(Without<Position>, Without<Velocity>)>,
    );
    let mut q = Query::<(Entity,), F>::new(&mut world);
    let mut hits: Vec<Entity> = q.iter_mut().map(|(e,)| e).collect();
    hits.sort_by_key(|e| e.id());

    // e_a: has A, lacks B → first Or via With<Position>, second Or via Without<Velocity> → match
    // e_b: lacks A, has B → first Or via With<Velocity>, second Or via Without<Position> → match
    // e_both: has both → first Or matches (both branches), but second Or fails both branches → no match
    let mut expected = vec![e_a, e_b];
    expected.sort_by_key(|e| e.id());
    assert_eq!(hits, expected);
}

#[test]
fn test_filter_single_element_tuple() {
    // (With<A>,) - single-element filter tuple should behave same as With<A>.
    let mut world = setup_world();
    let e_a = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();

    let mut q = Query::<(Entity,), (With<Position>,)>::new(&mut world);
    let hits: Vec<Entity> = q.iter_mut().map(|(e,)| e).collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0], e_a);
}

#[test]
fn test_filter_changed_empty_after_no_mutation() {
    let mut world = setup_world();
    world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();

    // Baseline at the current tick - the existing component's
    // `changed` tick is older, so the filter must yield nothing.
    world.set_system_last_run(world.change_tick());

    let mut q = Query::<(Entity,), Changed<Position>>::new(&mut world);
    assert_eq!(q.iter_mut().count(), 0);
}
