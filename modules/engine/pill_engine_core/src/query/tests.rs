//! Unit tests for the query system: basic queries, parallel iteration,
//! change detection, and filter predicates.
//!
//! # Responsibilities
//!
//! - Exercises the shared [`Position`], [`Velocity`], and [`Health`] fixture
//!   components through sequential and parallel query iteration, including
//!   tracked batch statistics.
//! - Verifies change-detection semantics of `Mut<T>` and the [`Added`] and
//!   [`Changed`] filters.
//! - Verifies archetype-scoping behaviour of the [`With`], [`Without`], and
//!   [`Or`] filter combinators.
//!
//! # Design
//!
//! Most tests build a fresh [`World`] through [`setup_world`] with the three
//! fixture components registered; the registration and shared-layout tests
//! build one directly. Filters are exercised at the archetype level ([`With`],
//! [`Without`], [`Or`]) and the row level ([`Added`], [`Changed`]), including
//! nested and mixed combinations that exercise [`Or`] short-circuiting and the
//! aliasing detection of the target tuples.

// External crates
use pill_core::rayon;

// Current crate
use super::*;
use crate::component::{Component, ComponentId};
use crate::entity::Entity;
use crate::test_fixtures::{setup_world, Health, Position, Velocity};
use crate::world::World;

// =============================================================================
// Tests
// =============================================================================

// =============================================================================
// Basic Query Tests
// =============================================================================

/// A query whose target component is never registered matches nothing and
/// panics nowhere: the unresolved id is recorded instead of dropped from the
/// mask, because a dropped id would make "not registered" mean "matches
/// everything".
#[test]
fn query_before_registration_matches_nothing() {
    let mut world = World::new();
    // `Position` is deliberately not registered here.
    let mut query = Query::<&Position>::new(&mut world);
    assert_eq!(query.iter_mut().count(), 0);

    // The mutable shape takes the same path.
    let mut query = Query::<&mut Position>::new(&mut world);
    assert_eq!(query.iter_mut().count(), 0);
}

/// `With<T>` for an unregistered `T` matches nothing rather than everything:
/// no archetype can hold a component that was never registered.
#[test]
fn an_unregistered_with_filter_matches_nothing() {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world
        .create_entity()
        .with(Position { x: 1.0, y: 2.0 })
        .build()
        .unwrap();

    let mut query = Query::<(Entity,), With<Health>>::new(&mut world);
    assert_eq!(
        query.iter_mut().count(),
        0,
        "an unregistered With<T> cannot match"
    );

    // Once the component is registered the filter answers its real question.
    world.register_component::<Health>();
    let mut query = Query::<(Entity,), With<Health>>::new(&mut world);
    assert_eq!(query.iter_mut().count(), 0, "the entity does not carry it");
    world.create_entity().with(Health(1)).build().unwrap();
    let mut query = Query::<(Entity,), With<Health>>::new(&mut world);
    assert_eq!(query.iter_mut().count(), 1);
}

#[test]
fn an_empty_world_matches_nothing() {
    let mut world = setup_world();
    let mut query = Query::<(&Position,)>::new(&mut world);
    assert_eq!(query.iter_mut().count(), 0);
}

#[test]
fn a_single_entity_yields_its_own_components() {
    let mut world = setup_world();
    world
        .create_entity()
        .with(Position { x: 1.0, y: 2.0 })
        .with(Velocity { x: 0.5, y: 0.5 })
        .build()
        .unwrap();

    let mut query = Query::<(&Position, &Velocity)>::new(&mut world);
    let results: Vec<_> = query.iter_mut().collect();

    assert_eq!(results.len(), 1, "one entity carries both components");
    let (position, velocity) = results[0];
    assert_eq!(
        position.x, 1.0,
        "the queried Position is the entity's own row"
    );
    assert_eq!(
        velocity.x, 0.5,
        "the queried Velocity is the entity's own row"
    );
}

#[test]
fn every_matching_entity_is_visited_once() {
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
fn an_entity_missing_a_requested_component_is_skipped() {
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
fn a_mutable_query_writes_through_to_the_row() {
    let mut world = setup_world();

    world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .with(Velocity { x: 1.0, y: 2.0 })
        .build()
        .unwrap();

    // Step 1: integrate velocity into position through a mutable query.
    {
        let mut query = Query::<(&mut Position, &Velocity)>::new(&mut world);
        for (mut pos, vel) in query.iter_mut() {
            pos.x += vel.x;
            pos.y += vel.y;
        }
    }

    // Step 2: read the modified position back through a fresh query and
    // assert the integration result.
    let mut query = Query::<(&Position,)>::new(&mut world);
    let (pos,) = query.first().unwrap();
    assert_eq!(pos.x, 1.0);
    assert_eq!(pos.y, 2.0);
}

#[test]
fn first_returns_the_first_matching_row() {
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
fn first_is_none_when_nothing_matches() {
    let mut world = setup_world();
    let mut query = Query::<(&Position,)>::new(&mut world);
    assert!(query.first().is_none());
}

/// `first()` agrees with `iter_mut().next()` even with several matching
/// archetypes.
///
/// Both walk the same sorted matching-archetype cache, so `first` always names
/// the row `iter_mut().next()` returns - a randomized archetype order would
/// let them disagree.
#[test]
fn first_agrees_with_iteration_order() {
    let mut world = setup_world();

    // Four distinct archetypes containing `Position`, each with its own `x`
    // so a disagreement about which archetype comes first is visible.
    let cases: [(&str, f32); 4] = [
        ("position", 11.0),
        ("velocity", 22.0),
        ("health", 33.0),
        ("both", 44.0),
    ];
    for (kind, x) in cases {
        let mut builder = world.create_entity().with(Position { x, y: 0.0 });
        if kind == "velocity" || kind == "both" {
            builder = builder.with(Velocity { x: 0.0, y: 0.0 });
        }
        if kind == "health" || kind == "both" {
            builder = builder.with(Health(1));
        }
        builder.build().unwrap();
    }

    let mut query = Query::<(&Position,)>::new(&mut world);
    let first_x = query.first().map(|(position,)| position.x);
    let iter_x = query.iter_mut().next().map(|(position,)| position.x);
    assert!(
        first_x.is_some(),
        "the world holds four matching archetypes"
    );
    assert_eq!(
        first_x, iter_x,
        "`first` must return the row `iter_mut().next()` returns"
    );
}

/// A filter with a row-level predicate and no archetype scoping produces no
/// filter pairs, which would read as "accepts everything" if the row-level
/// predicate were skipped: a filter rejecting every row must still answer
/// nothing from `first`, `is_empty` and `entity_count`.
#[test]
fn row_only_filters_are_not_skipped() {
    /// Rejects every row without declaring any component scope.
    struct RejectAll;
    impl QueryFilter for RejectAll {
        type State = ();
        fn init_state(
            _archetype: &crate::archetype::Archetype,
            _last_run: crate::Tick,
            _this_run: crate::Tick,
        ) -> Self::State {
        }
        fn matches(_state: &Self::State, _index: usize) -> bool {
            false
        }
    }

    let mut world = setup_world();
    for index in 0..5 {
        world
            .create_entity()
            .with(Position {
                x: index as f32,
                y: 0.0,
            })
            .build()
            .unwrap();
    }

    let mut query = Query::<(&Position,), RejectAll>::new(&mut world);
    assert!(query.first().is_none(), "a rejecting filter matches no row");
    assert!(query.is_empty(), "a rejecting filter matches no row");
    assert_eq!(query.entity_count(), 0, "a rejecting filter matches no row");
}

#[test]
fn the_entity_handle_is_part_of_the_query_data() {
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

// =============================================================================
// Parallel Iterator Tests
// =============================================================================

#[test]
fn parallel_iteration_reaches_every_matching_row() {
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

/// Verifies that explicit batch sizes are respected during parallel iteration.
///
/// The world is sized from the pool so the parallel path runs at all (a world
/// under `threads × MINIMUM_SLICE_SIZE` takes the sequential fallback), which
/// makes the geometry arithmetic instead of a lower bound: slices of exactly
/// 100, grouped contiguously across the pool.
#[test]
fn an_explicit_batch_size_controls_the_slices() {
    let mut world = setup_world();
    let num_threads = rayon::current_num_threads();
    let batch_size = 100usize;
    let entity_count = (num_threads * 1024).div_ceil(batch_size) * batch_size;

    for _ in 0..entity_count {
        world
            .create_entity()
            .with(Position { x: 0.0, y: 0.0 })
            .build()
            .unwrap();
    }

    let mut query = Query::<(&mut Position,)>::new(&mut world);
    let stats = query
        .par_iter_mut()
        .with_batch_size(batch_size)
        .tracked()
        .for_each(|(mut pos,)| {
            pos.x = 1.0;
        });

    let stats = stats.unwrap();
    assert_eq!(stats.total_entities, entity_count);

    // With every slice exactly `batch_size` long and the slices grouped
    // contiguously across the pool, each thread group's processed count is
    // `batch_size × (its slice count)`: `base` slices each, plus one more for
    // the first `remainder` groups. Ignoring `with_batch_size` changes the
    // slice lengths (the default spans 256..4096), so both equalities catch
    // it.
    let slices = entity_count / batch_size;
    let base = slices / num_threads;
    let remainder = slices % num_threads;
    assert_eq!(
        stats.batch_count, num_threads,
        "slices must be grouped contiguously across the pool"
    );
    assert_eq!(stats.min_batch_size, batch_size * base);
    assert_eq!(
        stats.max_batch_size,
        batch_size * (base + usize::from(remainder > 0))
    );
}

/// Verifies that tracked parallel iteration returns valid `BatchStats`.
#[test]
fn tracked_parallel_iteration_reports_its_batches() {
    let mut world = setup_world();

    for _ in 0..500 {
        world
            .create_entity()
            .with(Position { x: 0.0, y: 0.0 })
            .build()
            .unwrap();
    }

    let mut query = Query::<(&Position,)>::new(&mut world);
    let stats = query
        .par_iter_mut()
        .tracked()
        .for_each(|_| {})
        .expect("tracked iteration returns stats");

    assert_eq!(stats.total_entities, 500);
    assert!(stats.batch_count > 0);
    assert!(stats.num_threads > 0);
    assert!(stats.avg_batch_size > 0.0);
}

/// Verifies that untracked parallel iteration returns no stats.
#[test]
fn untracked_parallel_iteration_reports_no_stats() {
    let mut world = setup_world();

    world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();

    let mut query = Query::<(&Position,)>::new(&mut world);
    let result = query.par_iter_mut().for_each(|_| {});

    assert!(result.is_none());
}

#[test]
fn a_parallel_iterator_counts_its_entities() {
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

// =============================================================================
// BatchStats Tests
// =============================================================================

/// Verifies that `BatchStats` Display implementation produces readable output.
#[test]
fn batch_stats_render_their_counts() {
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

// =============================================================================
// QueryTarget Trait Tests
// =============================================================================

/// Verifies that `component_ids()` returns the correct set for a query target.
#[test]
fn a_query_reports_the_component_ids_it_matches() {
    let ids = <(&Position, &Velocity)>::component_ids();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&ComponentId::of::<Position>()));
    assert!(ids.contains(&ComponentId::of::<Velocity>()));
}

/// The mask a query builds and the access a system reports come from one
/// description: `component_ids()` is exactly `reads ++ writes`.
#[test]
fn the_reported_access_matches_the_component_ids() {
    let ids = <(&Position, &mut Velocity)>::component_ids();
    let (reads, writes) = <(&Position, &mut Velocity)>::report_component_access();

    let derived: Vec<ComponentId> = reads.into_iter().chain(writes).collect();
    assert_eq!(ids, derived);
    assert!(ids.contains(&ComponentId::of::<Position>()));
    assert!(ids.contains(&ComponentId::of::<Velocity>()));
}

/// Verifies that `report_component_access` correctly reports read-only access.
#[test]
fn a_read_access_is_reported_as_read() {
    let (reads, writes) = <(&Position,)>::report_component_access();
    assert_eq!(reads.len(), 1);
    assert_eq!(writes.len(), 0);
}

/// Verifies that `report_component_access` correctly reports write access.
#[test]
fn a_write_access_is_reported_as_write() {
    let (reads, writes) = <(&mut Position,)>::report_component_access();
    assert_eq!(reads.len(), 0);
    assert_eq!(writes.len(), 1);
}

/// Verifies that `report_component_access` correctly reports mixed read/write access.
#[test]
fn a_mixed_query_reports_both_access_modes() {
    let (reads, writes) = <(&Position, &mut Velocity)>::report_component_access();
    assert_eq!(reads.len(), 1);
    assert_eq!(writes.len(), 1);
}

/// Verifies that `report_component_access` refuses duplicate mutable
/// component types in every build, not only debug ones.
///
/// A `Query<(&mut Position, &mut Position)>` would create aliasing `&mut`
/// references to the same storage, which is undefined behaviour.
#[test]
#[should_panic(expected = "reads and writes the same component")]
fn two_writable_views_of_one_component_are_refused() {
    let _ = <(&mut Position, &mut Position)>::report_component_access();
}

/// Verifies the aliasing predicate: two writes of one id, or a read beside a
/// write, are found; duplicate reads are not aliasing.
#[test]
fn finding_a_component_detects_an_aliasing_target() {
    use crate::query::target::find_aliasing_component;
    let id = ComponentId::of::<Position>();
    let other = ComponentId::of::<Velocity>();
    assert_eq!(find_aliasing_component(&[], &[]), None);
    assert_eq!(find_aliasing_component(&[], &[id]), None);
    assert_eq!(find_aliasing_component(&[id, other], &[]), None);
    assert_eq!(
        find_aliasing_component(&[id, id], &[]),
        None,
        "two reads of one component are not aliasing"
    );
    assert_eq!(find_aliasing_component(&[], &[id, id]), Some(id));
    assert_eq!(find_aliasing_component(&[id], &[id]), Some(id));
    assert_eq!(find_aliasing_component(&[other], &[id]), None);
}

/// Construction is where aliasing is refused: `Query::new` runs the same
/// predicate registration does, so a directly built query cannot skip it.
#[test]
#[should_panic(expected = "reads and writes the same component")]
fn a_read_write_alias_is_refused_when_the_query_is_built() {
    let mut world = World::new();
    world.register_component::<Position>();
    let _ = Query::<(&Position, &mut Position)>::new(&mut world);
}

/// A duplicated read is not aliasing and must keep constructing fine.
#[test]
fn two_read_views_of_one_component_are_allowed() {
    let mut world = World::new();
    world.register_component::<Position>();
    let mut query = Query::<(&Position, &Position)>::new(&mut world);
    assert_eq!(query.iter_mut().count(), 0);
}

/// Verifies that `Entity` reports zero component IDs.
#[test]
fn an_entity_only_query_names_no_component() {
    let ids = Entity::component_ids();
    assert!(ids.is_empty());
}

// =============================================================================
// Change Detection Tests
// =============================================================================

/// Verifies that `DerefMut` on `Mut<T>` bumps the changed tick.
#[test]
fn writing_through_a_mutable_view_bumps_the_changed_tick() {
    let mut world = setup_world();
    world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();

    // Step 1: capture the tick at which the component was added (insert
    // happened with whatever world.change_tick was at the time).
    let added_tick = {
        let mut q = Query::<(&mut Position,)>::new(&mut world);
        let (m,) = q.first().unwrap();
        m.last_added()
    };

    // Step 2: mutate via DerefMut - should bump `changed` to the
    // iterator's this_run, which is strictly newer than `added`.
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

/// Verifies that immutable `Deref` on `Mut<T>` does not bump the changed tick.
#[test]
fn reading_through_a_shared_view_leaves_the_changed_tick() {
    let mut world = setup_world();
    world
        .create_entity()
        .with(Position { x: 1.0, y: 2.0 })
        .build()
        .unwrap();

    // Step 1: record the changed tick before any dereference.
    let baseline = {
        let mut q = Query::<(&mut Position,)>::new(&mut world);
        let (m,) = q.first().unwrap();
        m.last_changed()
    };

    // Step 2: read through an immutable deref - the changed tick must
    // not advance.
    let after_read = {
        let mut q = Query::<(&mut Position,)>::new(&mut world);
        let (m,) = q.first().unwrap();
        let _x = m.x; // immutable deref
        m.last_changed()
    };

    assert_eq!(baseline, after_read);
}

/// Verifies that `bypass_change_detection()` provides raw access without
/// bumping change-detection ticks.
#[test]
fn a_bypassing_read_leaves_the_changed_tick() {
    let mut world = setup_world();
    world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();

    // Step 1: record the changed tick before any mutation.
    let baseline = {
        let mut q = Query::<(&mut Position,)>::new(&mut world);
        let (m,) = q.first().unwrap();
        m.last_changed()
    };

    // Step 2: mutate through `bypass_change_detection` - the changed
    // tick must not advance.
    let after_bypass = {
        let mut q = Query::<(&mut Position,)>::new(&mut world);
        let (mut m,) = q.first().unwrap();
        m.bypass_change_detection().x = 99.0;
        m.last_changed()
    };

    assert_eq!(baseline, after_bypass);

    // Step 3: verify the raw mutation still took effect.
    let mut verify = Query::<(&Position,)>::new(&mut world);
    let (pos,) = verify.first().unwrap();
    assert_eq!(pos.x, 99.0);
}

/// Verifies that `Added` ticks survive archetype migration.
#[test]
fn the_added_tick_survives_an_archetype_move() {
    let mut world = setup_world();
    let entity = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();

    // Step 1: record the `added` tick of Position before migration.
    let original_added = {
        let mut q = Query::<(&mut Position,)>::new(&mut world);
        let (m,) = q.first().unwrap();
        m.last_added()
    };

    // Step 2: migrate to a new archetype by adding a component. The
    // Position component carries over and its `added` tick must be
    // preserved.
    world
        .add_component(entity, Velocity { x: 1.0, y: 1.0 })
        .unwrap();

    // Step 3: verify the migrated Position preserves its `added` tick.
    let after_migration = {
        let mut q = Query::<(&mut Position,)>::new(&mut world);
        let (m,) = q.first().unwrap();
        m.last_added()
    };

    assert_eq!(original_added, after_migration);

    // Step 4: verify the newly attached Velocity has an `added` tick at
    // least as new as the original Position's.
    let velocity_added = {
        let mut q = Query::<(&mut Velocity,)>::new(&mut world);
        let (m,) = q.first().unwrap();
        m.last_added()
    };
    assert!(velocity_added >= original_added);
}

// =============================================================================
// QueryFilter Tests
// =============================================================================

/// Verifies that `With<T>` filter only matches archetypes containing T.
#[test]
fn a_with_filter_skips_archetypes_without_the_component() {
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

/// Verifies that `Without<T>` filter excludes archetypes containing T.
#[test]
fn a_without_filter_skips_archetypes_that_have_the_component() {
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

/// Verifies that `Changed<T>` filter only yields rows whose component was mutated.
#[test]
fn a_changed_filter_sees_only_mutated_rows() {
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

    // Step 1: set the baseline so all "Added" ticks are in the past.
    world.set_system_last_run(world.change_tick());

    // Step 2: mutate only e1 so its `changed` tick advances.
    {
        let mut q = Query::<(Entity, &mut Position)>::new(&mut world);
        for (entity, mut pos) in q.iter_mut() {
            if entity == e1 {
                pos.x = 99.0;
            }
        }
    }

    // Step 3: filter on `Changed<Position>` - only e1 should appear.
    let mut q = Query::<(Entity, &Position), Changed<Position>>::new(&mut world);
    let hits: Vec<Entity> = q.iter_mut().map(|(e, _)| e).collect();
    assert_eq!(hits, vec![e1]);
}

/// Verifies that `Added<T>` filter only yields rows whose component was newly inserted.
#[test]
fn an_added_filter_sees_only_new_rows() {
    let mut world = setup_world();
    let _old = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();

    // Step 1: lock in the current tick as the baseline so the existing
    // entity is NOT considered "added" relative to it.
    world.set_system_last_run(world.change_tick());

    // Step 2: bump the tick (simulating a frame boundary) and insert a
    // new entity - it should be the only `Added<Position>` hit.
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

/// Verifies that `Or` filter combines predicates with logical OR semantics.
#[test]
fn an_or_filter_matches_either_branch() {
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

    // Step 1: mutate Position on e1 and Velocity on e2 so each entity
    // triggers a different `Changed` branch.
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

    // Step 2: query with `Or<(Changed<Position>, Changed<Velocity>)>` -
    // both entities should match via their respective branches.
    let mut q = Query::<(Entity,), Or<(Changed<Position>, Changed<Velocity>)>>::new(&mut world);
    let mut hits: Vec<Entity> = q.iter_mut().map(|(e,)| e).collect();
    hits.sort_by_key(|e| e.id());
    let mut expected = vec![e1, e2];
    expected.sort_by_key(|e| e.id());
    assert_eq!(hits, expected);
}

/// Verifies that `Or<(With<A>, With<B>)>` matches entities with either component.
#[test]
fn an_or_of_two_with_filters_matches_either_component() {
    // `Or<(With<A>, With<B>)>` must match archetypes containing A *or* B (not
    // just both): the pairs are `({A},{})` and `({B},{})`, so unioning the
    // inner includes into one pair would wrongly require both.
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

/// Verifies that `Or` with `With` and `Without` filters correct archetypes.
#[test]
fn an_or_of_with_and_without_filters_picks_the_right_archetypes() {
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

/// Verifies that `Or` mixed with `Changed` correctly combines archetype and row-level filtering.
#[test]
fn an_or_branch_combined_with_changed_keeps_both_rules() {
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

/// Verifies that `Or<(Without<A>, Without<B>)>` correctly excludes archetypes with either component.
#[test]
fn an_or_of_two_without_filters_matches_everything_else() {
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

/// Verifies that a three-way `Or` filter correctly matches all three branches.
#[test]
fn a_three_way_or_matches_any_branch() {
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

/// Verifies that `Or` within an AND tuple correctly narrows the result set.
#[test]
fn an_or_nested_inside_an_and_tuple_still_applies() {
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

/// Verifies that an `Or`-scoped query whose target is only `Entity` yields the
/// entities its branches match.
#[test]
fn an_or_with_an_empty_branch_matches_nothing_from_it() {
    // Query<(Entity,), Or<(With<A>, With<B>)>>: no component data is fetched,
    // so the `Or` filter alone decides which archetypes match.
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

    let mut q = Query::<(Entity,), Or<(With<Position>, With<Velocity>)>>::new(&mut world);
    let mut hits: Vec<Entity> = q.iter_mut().map(|(e,)| e).collect();
    hits.sort_by_key(|e| e.id());

    let mut expected = vec![e_a, e_b];
    expected.sort_by_key(|e| e.id());
    assert_eq!(hits, expected);
}

/// Verifies that `Or` filters do not double-count entities matching multiple branches.
#[test]
fn an_or_matching_both_branches_visits_a_row_once() {
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

/// Verifies that `Or` filters work correctly with parallel iteration.
#[test]
fn an_or_filter_applies_on_the_parallel_path() {
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
    // Every matching entity exactly once: this is the test that drives
    // `F::init_state` through the parallel closure, so a slice visited twice
    // or skipped altogether changes the count above.
    let mut distinct = std::collections::HashSet::new();
    for entity in &hits {
        assert!(
            distinct.insert(entity.id()),
            "an entity was visited by two slices"
        );
    }
}

/// Verifies that `Or` with a unit `()` branch is always true.
#[test]
fn an_or_with_a_unit_branch_matches_every_row() {
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

/// Verifies that `Or` with `()` as the first branch correctly short-circuits.
#[test]
fn a_unit_branch_first_still_matches_every_row() {
    // Or<((), With<A>)>: order of branches shouldn't matter. The ()
    // branch (first position) still makes the whole Or match everything.
    let mut world = setup_world();
    let e_any = world.create_entity().with(Health(10)).build().unwrap();

    let mut q = Query::<(Entity,), Or<((), With<Position>)>>::new(&mut world);
    let hits: Vec<Entity> = q.iter_mut().map(|(e,)| e).collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0], e_any);
}

/// Verifies that `Or` with duplicate inner filters still produces correct results.
#[test]
fn a_repeated_inner_filter_matches_once() {
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

/// Verifies that AND of two `Or` filters correctly intersects their result sets.
#[test]
fn an_and_of_two_ors_requires_both() {
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

/// Verifies that a single-element filter tuple works identically to the bare filter.
#[test]
fn a_one_element_filter_tuple_still_filters() {
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

/// Verifies that `Changed<T>` returns no rows when no mutation has occurred.
#[test]
fn a_changed_filter_is_empty_when_nothing_was_written() {
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

// =============================================================================
// Stale shared readers
// =============================================================================

/// The shared name the three point types below all register under, as the
/// same component compiled into different binaries would.
const SHARED_POINT: &str = "query_tests::SharedPoint";

/// The layout the registry holds: `x`, then `y`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, pill_engine_macros::PillLayout)]
struct CurrentPoint {
    x: f32,
    y: f32,
}
impl Component for CurrentPoint {
    fn shared_name() -> Option<&'static str> {
        Some(SHARED_POINT)
    }
    fn declared_schema_hash() -> Option<u64> {
        Some(crate::component::component_schema_hash(Self::FIELD_LAYOUT))
    }
}

/// A reader built against an older layout of the same component: same size and
/// alignment, fields swapped - what size and alignment alone cannot catch.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, pill_engine_macros::PillLayout)]
struct StalePoint {
    y: f32,
    x: f32,
}
impl Component for StalePoint {
    fn shared_name() -> Option<&'static str> {
        Some(SHARED_POINT)
    }
    fn declared_schema_hash() -> Option<u64> {
        Some(crate::component::component_schema_hash(Self::FIELD_LAYOUT))
    }
}

/// A reader that declares no layout, so only size and alignment are checked.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct UndeclaredPoint {
    x: f32,
    y: f32,
}
impl Component for UndeclaredPoint {
    fn shared_name() -> Option<&'static str> {
        Some(SHARED_POINT)
    }
}

/// A world holding one entity of the shared point, registered with
/// [`CurrentPoint`]'s layout.
fn world_with_a_current_point() -> World {
    let mut world = World::new();
    world.register_component_with_layout::<CurrentPoint>(CurrentPoint::FIELD_LAYOUT);
    world
        .create_entity()
        .with(CurrentPoint { x: 1.0, y: 2.0 })
        .build()
        .unwrap();
    world
}

/// A query whose type declares a different layout of a shared component than
/// the registered one is refused and matches nothing, alone, mutable, or inside
/// a tuple; the current layout's query still sees the row.
#[test]
fn a_stale_shared_reader_is_refused() {
    let mut world = world_with_a_current_point();
    let point = ComponentId::of::<CurrentPoint>();
    assert_eq!(
        ComponentId::of::<StalePoint>(),
        point,
        "one shared name, one component"
    );

    let mut stale = Query::<&StalePoint>::new(&mut world);
    assert_eq!(stale.stale_shared_component(), Some(point));
    assert_eq!(stale.iter_mut().count(), 0);

    let mut stale_mutable = Query::<&mut StalePoint>::new(&mut world);
    assert_eq!(stale_mutable.stale_shared_component(), Some(point));
    assert_eq!(stale_mutable.iter_mut().count(), 0);

    let mut stale_in_tuple = Query::<(Entity, &StalePoint)>::new(&mut world);
    assert_eq!(stale_in_tuple.stale_shared_component(), Some(point));
    assert_eq!(stale_in_tuple.entity_count(), 0);

    let mut current = Query::<&CurrentPoint>::new(&mut world);
    assert_eq!(current.stale_shared_component(), None);
    let values: Vec<(f32, f32)> = current.iter_mut().map(|point| (point.x, point.y)).collect();
    assert_eq!(values, [(1.0, 2.0)]);
}

/// A reader declaring no layout uses the size-and-alignment check alone, so it
/// still matches.
#[test]
fn a_reader_without_a_declared_layout_falls_back_to_size_and_alignment() {
    let mut world = world_with_a_current_point();
    let mut query = Query::<&UndeclaredPoint>::new(&mut world);
    assert_eq!(query.stale_shared_component(), None);
    assert_eq!(query.iter_mut().count(), 1);
}

/// A refused query is rebuilt every frame; the refusal is recorded once.
#[test]
fn a_stale_reader_is_reported_once() {
    let mut world = world_with_a_current_point();
    for _ in 0..3 {
        let _ = Query::<&StalePoint>::new(&mut world);
    }
    assert_eq!(world.reported_schema_mismatches.len(), 1);
}

/// A zero batch size cannot reach the slice arithmetic.
///
/// `min_len` is a divisor in both executors, so a caller-computed zero would
/// divide by zero inside `for_each` and take the host down mid-frame. The
/// setter clamps, so the zero becomes the finest split the executor can make.
#[test]
fn a_zero_batch_size_does_not_panic() {
    let mut world = setup_world();
    let num_threads = rayon::current_num_threads();
    let entity_count = num_threads * 1024;
    for _ in 0..entity_count {
        world
            .create_entity()
            .with(Position { x: 0.0, y: 0.0 })
            .build()
            .unwrap();
    }

    // Enough entities to clear the sequential fallback, so the slice division
    // is reached rather than skipped.
    let mut query = Query::<(&mut Position,)>::new(&mut world);
    let stats = query
        .par_iter_mut()
        .with_batch_size(0)
        .tracked()
        .for_each(|(mut pos,)| {
            pos.x = 1.0;
        })
        .unwrap();

    assert_eq!(stats.total_entities, entity_count);
    // Slices of one entity, grouped contiguously across the pool, so every
    // thread group covers exactly the entities it was given.
    assert_eq!(stats.min_batch_size, entity_count / num_threads);
    assert_eq!(stats.max_batch_size, entity_count / num_threads);
}
