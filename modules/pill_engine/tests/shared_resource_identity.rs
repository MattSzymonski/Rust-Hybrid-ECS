//! Integration tests for resources that keep one identity across artifacts.
//!
//! # What is being simulated
//!
//! The situation cannot be reproduced literally in one test binary: it needs
//! one resource type compiled into two artifacts loaded into the same process,
//! which is what gives that single type two `TypeId`s.
//!
//! Two distinct Rust types declaring the *same* shared name reproduce it
//! faithfully, because that is exactly what the engine sees in the real case -
//! two `TypeId`s, one declared identity, identical layouts - and it is the only
//! thing the engine ever sees of it. The two live in modules and share the
//! type's own name, as the real case does: `Settings` is `Settings` in whichever
//! artifact compiled it, and only the module path differs.
//!
//! The cross-artifact behaviour itself is covered end to end by
//! `devops/tests/test_shared_component_identity.py`, whose resource scenario
//! inserts from a module DLL and reads from the project, and whose writers make
//! the scheduler keep them apart.
//!
//! The scheduler side is covered at five levels here: the pairwise answer
//! (`conflicts_with`), the fallback when a caller mutates the sets after
//! `build_component_masks`, the batching decision built on top of both
//! (`SystemScheduler::build_execution_graph`), the rebuild a reload's `retain`
//! does, and the running engine, where two systems that were batched together
//! would trip the debug write lock.

use pill_engine::query::{Res, ResMut};
use pill_engine::scheduler::{SystemAccess, SystemScheduler};
use pill_engine::{Engine, Resource, ResourceId, World};

// =============================================================================
// Stand-ins
// =============================================================================

/// One artifact's copy of the shared resource.
pub mod artifact_a {
    /// `demo::Settings` as one artifact compiled it.
    #[derive(Debug)]
    pub struct Settings {
        pub value: u32,
    }
    impl pill_engine::Resource for Settings {
        fn shared_name() -> Option<&'static str> {
            Some("demo::Settings")
        }
    }
}

/// The other artifact's copy: a different Rust type, hence a different
/// `TypeId`, declaring the same shared name and the same layout.
pub mod artifact_b {
    /// `demo::Settings` as the other artifact compiled it.
    #[derive(Debug)]
    pub struct Settings {
        pub value: u32,
    }
    impl pill_engine::Resource for Settings {
        fn shared_name() -> Option<&'static str> {
            Some("demo::Settings")
        }
    }
}

/// A third artifact built against a *changed* definition: same name, same
/// declared identity, wider fields. The name guard passes; the layout guard
/// must not.
pub mod stale_artifact {
    /// `demo::Settings` as an artifact rebuilt against a changed definition
    /// compiled it.
    #[derive(Debug)]
    pub struct Settings {
        pub value: u64,
        pub extra: u64,
    }
    impl pill_engine::Resource for Settings {
        fn shared_name() -> Option<&'static str> {
            Some("demo::Settings")
        }
    }
}

/// An unrelated resource claiming the same shared name, which the name guard
/// must separate from the legitimate copies above.
#[derive(Debug)]
struct UnrelatedSettings {
    _value: u32,
}
impl Resource for UnrelatedSettings {
    fn shared_name() -> Option<&'static str> {
        Some("demo::Settings")
    }
}

/// One artifact's copy of a hashed resource: same name, same size, one field
/// shape.
pub mod hashed_a {
    /// `demo::Hashed` as one artifact compiled it.
    pub struct Settings {
        pub value: u32,
    }
    impl pill_engine::Resource for Settings {
        fn shared_name() -> Option<&'static str> {
            Some("demo::Hashed")
        }
        fn shared_schema_hash() -> Option<u64> {
            Some(0x1111)
        }
    }
}

/// The other artifact's copy of the same name: `{u32}` reinterpreted as another
/// shape that agrees on size and alignment, which only the hooks can tell apart.
pub mod hashed_b {
    /// `demo::Hashed` as the other artifact compiled it.
    pub struct Settings {
        pub value: u32,
    }
    impl pill_engine::Resource for Settings {
        fn shared_name() -> Option<&'static str> {
            Some("demo::Hashed")
        }
        fn shared_schema_hash() -> Option<u64> {
            Some(0x2222)
        }
    }
}

/// A Rust resource declaring the foreign slot's name at another shape, so its
/// id collides with a value it cannot be read as.
pub mod identity_probe {
    /// `identity::Guarded`, declared wider than the foreign payload.
    #[derive(Debug)]
    pub struct Guarded {
        pub first: u32,
        pub second: u32,
    }
    impl pill_engine::Resource for Guarded {
        fn shared_name() -> Option<&'static str> {
            Some("identity::Guarded")
        }
    }
}

/// An ordinary resource, to show the default identity is untouched.
#[derive(Debug, Default)]
struct PlainCounter {
    value: u32,
}
impl Resource for PlainCounter {}

/// A relayout moves the claim with the factory: the migrated shape can be
/// re-declared, and the shape it came from is refused as stale.
#[test]
fn a_relayout_releases_the_new_layout() {
    let mut world = World::new();
    let id = world
        .register_foreign_resource("identity::Reshaped", "Reshaped", 16, 8, 1)
        .expect("a fresh name is claimed");

    let migrated = world
        .relayout_foreign_resource(
            id,
            32,
            8,
            2,
            &pill_engine::archetype::DynamicFieldPlan::new(),
        )
        .expect("the empty plan fits");
    assert_eq!(migrated, 0, "no value was stored yet");

    world
        .register_foreign_resource("identity::Reshaped", "Reshaped", 32, 8, 2)
        .expect("the migrated layout re-declares");
    let error = world
        .register_foreign_resource("identity::Reshaped", "Reshaped", 16, 8, 1)
        .expect_err("the pre-migration layout is stale");
    assert!(matches!(
        error,
        pill_engine::error::WorldError::SharedResourceLayoutMismatch { .. }
    ));
}

/// A refused removal is a no-op: the take is attempted before anything is
/// torn down, and the box it hands back is put where it was.
#[test]
fn a_refused_removal_leaves_everything_in_place() {
    let mut world = World::new();
    let id = world
        .register_foreign_resource("identity::Guarded", "Guarded", 4, 4, 7)
        .expect("a fresh name is claimed");
    world
        .insert_foreign_resource_bytes(id, &9_u32.to_ne_bytes())
        .expect("the payload matches the size");

    // `stale_artifact::Settings` declares `demo::Settings`; this probe declares
    // `identity::Guarded` at eight bytes instead of four, so its take has to
    // fail while everything under the id stays standing.
    let error = world
        .remove_resource::<identity_probe::Guarded>()
        .expect_err("a differently-shaped Rust type cannot take the value");
    assert!(matches!(
        error,
        pill_engine::error::WorldError::SharedResourceHoldsAnotherType { id: reported, .. }
            if reported == id
    ));

    assert_eq!(
        world.foreign_resource_bytes(id),
        Some(9_u32.to_ne_bytes().as_slice()),
        "the value is still there"
    );
    assert_eq!(world.foreign_resource_layout(id), Some((4, 4, 7)));
    assert_eq!(
        world.shared_resource_names(),
        vec!["identity::Guarded".to_string()],
        "the claim is still recorded"
    );
}

/// The existence query answers readability, not id-presence: a foreign shape
/// under a shared id is not a `T`, so `has_resource` agrees with
/// `get_resource`, and `resource_holder` is what answers "something is there".
#[test]
fn has_resource_reports_the_readable_shape_not_the_id() {
    let mut world = World::new();
    let id = world
        .register_foreign_resource("demo::Settings", "Project.Settings", 4, 4, 7)
        .expect("the name is claimed");
    world
        .insert_foreign_resource_bytes(id, &7_u32.to_ne_bytes())
        .expect("the payload matches");

    // `stale_artifact::Settings` declares the same name at sixteen bytes.
    assert!(
        !world.has_resource::<stale_artifact::Settings>(),
        "id presence must not answer for a shape the value cannot be read as"
    );
    assert!(world.get_resource::<stale_artifact::Settings>().is_none());
    assert!(
        world
            .resource_holder::<stale_artifact::Settings>()
            .is_some(),
        "the holder accessor is what names the stored shape"
    );

    // The readable copy still answers both questions the same way.
    assert!(world.has_resource::<artifact_a::Settings>());
    assert!(world.get_resource::<artifact_a::Settings>().is_some());
}

/// Two copies whose declared fields disagree are refused even though name,
/// size and alignment all agree: the schema hook is the only evidence that can
/// tell a reinterpretation from a matching pair.
#[test]
fn differing_shapes_under_one_name_are_refused() {
    let mut world = World::new();
    world.register_resource::<hashed_a::Settings>();
    assert!(world.take_registration_error().is_none());

    world.register_resource::<hashed_b::Settings>();
    let error = world
        .take_registration_error()
        .expect("the second shape is refused");
    assert!(matches!(
        error,
        pill_engine::error::WorldError::SharedResourceSchemaMismatch { .. }
    ));
}

/// A second ordinary resource, for the disjointness half of the scheduler test.
#[derive(Debug, Default)]
struct OtherCounter {
    _value: u32,
}
impl Resource for OtherCounter {}

// =============================================================================
// Identity
// =============================================================================

/// Both copies resolve to the same [`ResourceId`], derived from the declared
/// name rather than from either artifact's `TypeId`.
#[test]
fn both_copies_of_a_shared_resource_resolve_to_one_id() {
    let a = ResourceId::of::<artifact_a::Settings>();
    let b = ResourceId::of::<artifact_b::Settings>();

    assert_eq!(a, b, "the two copies must be one resource");
    assert!(a.shared_identity().is_some());

    // A resource declaring nothing keeps the per-artifact identity.
    assert!(ResourceId::of::<PlainCounter>().shared_identity().is_none());
    assert_ne!(
        ResourceId::of::<PlainCounter>(),
        ResourceId::of::<OtherCounter>()
    );
}

// =============================================================================
// Access across the boundary
// =============================================================================

/// The decisive test: one artifact inserts, the other reads it back.
#[test]
fn a_resource_inserted_by_one_copy_is_read_by_the_other() {
    let mut world = World::new();
    world.insert_resource(artifact_a::Settings { value: 4242 });

    let seen = world
        .get_resource::<artifact_b::Settings>()
        .expect("the other artifact's copy reaches the same slot");
    assert_eq!(seen.value, 4242);
}

/// Writes travel the same way.
#[test]
fn a_write_through_one_copy_is_seen_by_the_other() {
    let mut world = World::new();
    world.insert_resource(artifact_a::Settings { value: 1 });

    world
        .get_resource_mut::<artifact_b::Settings>()
        .expect("reachable through the other copy")
        .value = 99;

    assert_eq!(
        world.get_resource::<artifact_a::Settings>().unwrap().value,
        99
    );
}

/// Presence is reported through either copy.
#[test]
fn presence_is_reported_through_either_copy() {
    let mut world = World::new();
    assert!(!world.has_resource::<artifact_b::Settings>());

    world.insert_resource(artifact_a::Settings { value: 5 });

    assert!(world.has_resource::<artifact_a::Settings>());
    assert!(world.has_resource::<artifact_b::Settings>());
}

/// The value can be taken out through the other copy, which is the one path
/// where ownership leaves the store.
#[test]
fn a_resource_can_be_removed_through_the_other_copy() {
    let mut world = World::new();
    world.insert_resource(artifact_a::Settings { value: 7 });

    let taken = world
        .remove_resource::<artifact_b::Settings>()
        .expect("the removal is not refused")
        .expect("the other copy can take ownership");
    assert_eq!(taken.value, 7);

    assert!(!world.has_resource::<artifact_a::Settings>());
    assert!(!world.has_resource::<artifact_b::Settings>());
}

/// A change tick written through one copy is observed through the other, so
/// change detection does not fragment along the artifact boundary.
#[test]
fn a_change_tick_written_by_one_copy_is_seen_by_the_other() {
    let mut world = World::new();
    world.insert_resource(artifact_a::Settings { value: 1 });

    let before = world.change_tick();
    world.increment_change_tick();

    // The other artifact mutates through the tracked path.
    world
        .get_resource_mut_tracked::<artifact_b::Settings>()
        .expect("reachable through the other copy")
        .value = 2;

    assert_eq!(
        world.get_resource::<artifact_a::Settings>().unwrap().value,
        2
    );
    assert_ne!(
        world.change_tick(),
        before,
        "the world tick moved, so the write is attributable"
    );
}

// =============================================================================
// System parameters
// =============================================================================

/// `Res<T>` and `ResMut<T>` resolve a shared resource across copies, which is
/// what a module's systems actually use.
#[test]
fn system_parameters_resolve_a_shared_resource_across_copies() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    let observed = Arc::new(AtomicU32::new(0));

    // One artifact's system writes it.
    fn writer(mut settings: ResMut<artifact_a::Settings>) {
        if let Some(mut settings) = settings.get_mut() {
            settings.value += 1;
        }
    }

    // The other artifact's system reads it.
    let seen = Arc::clone(&observed);
    let reader = move |settings: Res<artifact_b::Settings>| {
        if let Some(settings) = settings.get() {
            seen.store(settings.value, Ordering::SeqCst);
        }
    };

    let mut engine = Engine::new();
    engine
        .world_mut()
        .insert_resource(artifact_a::Settings { value: 10 });
    engine.register_system("writer", writer);
    engine.register_system("reader", reader);

    engine.process_frame().unwrap();

    assert_eq!(
        engine
            .world()
            .get_resource::<artifact_b::Settings>()
            .unwrap()
            .value,
        11,
        "the write landed on the shared slot"
    );
    assert_eq!(
        observed.load(Ordering::SeqCst),
        11,
        "the other copy's system read the written value"
    );
}

// =============================================================================
// Scheduler soundness
// =============================================================================

/// Two systems writing one shared resource through different copies must
/// report a conflict, or the scheduler would batch them in parallel and they
/// would race on the same value.
///
/// This is a soundness property rather than a correctness one: `conflicts_with`
/// compares resource ids, so two ids for one resource would make the two
/// systems compare as disjoint.
#[test]
fn two_systems_writing_through_different_copies_conflict() {
    let mut world = World::new();
    world.insert_resource(artifact_a::Settings { value: 0 });
    world.insert_resource(PlainCounter::default());
    world.insert_resource(OtherCounter::default());
    let registry = world.component_registry();

    let mut writer_a = SystemAccess::new();
    writer_a.add_resource_write(ResourceId::of::<artifact_a::Settings>());
    writer_a.build_component_masks(registry);

    let mut writer_b = SystemAccess::new();
    writer_b.add_resource_write(ResourceId::of::<artifact_b::Settings>());
    writer_b.build_component_masks(registry);

    assert!(
        writer_a.conflicts_with(&writer_b),
        "two writes to one shared resource must never be batched in parallel"
    );

    // A write and a read of the same shared resource conflict as well.
    let mut reader_b = SystemAccess::new();
    reader_b.add_resource_read(ResourceId::of::<artifact_b::Settings>());
    reader_b.build_component_masks(registry);
    assert!(writer_a.conflicts_with(&reader_b));

    // Unrelated resources still do not conflict, so the check has not simply
    // become "everything conflicts".
    let mut unrelated = SystemAccess::new();
    unrelated.add_resource_write(ResourceId::of::<OtherCounter>());
    unrelated.build_component_masks(registry);
    assert!(!writer_a.conflicts_with(&unrelated));
}

/// A resource id registered *after* the masks were built must still conflict.
///
/// `conflicts_with` trusts its sorted snapshot only while it mirrors the live
/// set, and falls back to the sets otherwise. Nothing else pins that fallback
/// for resources, and a mistyped length check there would turn two writers of
/// one resource into a parallel batch - silently, because the sets are correct
/// and only the shortcut is wrong.
#[test]
fn a_resource_registered_after_the_masks_still_conflicts() {
    let world = World::new();
    let registry = world.component_registry();

    let mut first = SystemAccess::new();
    first.add_resource_write(ResourceId::of::<artifact_a::Settings>());
    first.build_component_masks(registry);

    // Built empty, then mutated: the snapshot is stale, the set is not.
    let mut second = SystemAccess::new();
    second.build_component_masks(registry);
    second.add_resource_write(ResourceId::of::<artifact_b::Settings>());

    assert!(
        first.conflicts_with(&second),
        "the second access's snapshot is stale, so the sets have to decide"
    );
}

/// The batching decision itself, not just the pairwise answer it rests on: two
/// systems that write one shared resource through different copies must be
/// placed in different batches.
///
/// This is the path a real engine takes - `Engine::register_system` adds the
/// resources, then calls `build_component_masks`, which snapshots them for the
/// allocation-free comparison - so the graph below is decided by the snapshots
/// and not by the sets. It asserts the third system's placement too: a resource
/// conflict must serialize the two writers, not push everything else out of
/// every batch with them.
#[test]
fn two_systems_writing_one_shared_resource_are_never_batched_together() {
    let world = World::new();
    let registry = world.component_registry();
    let mut scheduler = SystemScheduler::new();

    let mut project_writer = SystemAccess::new();
    project_writer.add_resource_write(ResourceId::of::<artifact_a::Settings>());
    project_writer.build_component_masks(registry);
    let project_system = scheduler.register_system(project_writer);

    let mut module_writer = SystemAccess::new();
    module_writer.add_resource_write(ResourceId::of::<artifact_b::Settings>());
    module_writer.build_component_masks(registry);
    let module_system = scheduler.register_system(module_writer);

    let mut unrelated_writer = SystemAccess::new();
    unrelated_writer.add_resource_write(ResourceId::of::<OtherCounter>());
    unrelated_writer.build_component_masks(registry);
    let unrelated_system = scheduler.register_system(unrelated_writer);

    scheduler.build_execution_graph();
    let batches = scheduler.execution_graph();

    let batch_of = |system: usize| batches.iter().position(|batch| batch.contains(&system));
    let project_batch = batch_of(project_system).expect("project writer scheduled");
    let module_batch = batch_of(module_system).expect("module writer scheduled");
    let unrelated_batch = batch_of(unrelated_system).expect("unrelated writer scheduled");

    assert_ne!(
        project_batch, module_batch,
        "two writers of one shared resource were batched together: {batches:?}"
    );
    assert!(
        unrelated_batch == project_batch || unrelated_batch == module_batch,
        "a system on an unrelated resource must keep running beside one writer: {batches:?}"
    );
}

/// The third level, and the one a reload takes: `retain` drops the reloaded
/// generation's systems and *rebuilds* the conflict matrix from the survivors
/// rather than filtering it in place.
///
/// A shared resource has to come out of that rebuild conflicting the way it went
/// in, or the two writers would start sharing a batch from the first frame after
/// the swap - which is the frame after the identities were proved to work.
///
/// The two survivors register the way `Engine::register_system` does, masks
/// included, so the rebuild compares snapshots the same way registration did.
#[test]
fn a_shared_resource_still_conflicts_after_retaining_systems() {
    let world = World::new();
    let registry = world.component_registry();
    let mut scheduler = SystemScheduler::new();

    let mut project_writer = SystemAccess::new();
    project_writer.add_resource_write(ResourceId::of::<artifact_a::Settings>());
    project_writer.build_component_masks(registry);
    let project_system = scheduler.register_system(project_writer);

    let mut module_writer = SystemAccess::new();
    module_writer.add_resource_write(ResourceId::of::<artifact_b::Settings>());
    module_writer.build_component_masks(registry);
    let module_system = scheduler.register_system(module_writer);

    // Stands in for a generation the reload drops. Registered last, so the two
    // writers keep their indices through the filter - the reindexing is the
    // caller's half of the contract and not what this test is about.
    let doomed_system = scheduler.register_system(SystemAccess::new());

    // What the host passes: one flag per system, false for the dropped one.
    let mut keep = vec![true; doomed_system + 1];
    keep[doomed_system] = false;
    scheduler.retain(&keep);

    // `retain` clears the graph, so the host rebuilds it after the swap.
    scheduler.build_execution_graph();
    let batches = scheduler.execution_graph();

    let batch_of = |system: usize| batches.iter().position(|batch| batch.contains(&system));
    assert!(
        batch_of(doomed_system).is_none(),
        "the dropped system is still scheduled: {batches:?}"
    );
    let project_batch = batch_of(project_system).expect("the surviving writer is scheduled");
    let module_batch = batch_of(module_system).expect("the surviving writer is scheduled");
    assert_ne!(
        project_batch, module_batch,
        "the shared conflict did not survive the rebuild: {batches:?}"
    );
}

/// The property through the path a system really takes: two systems whose
/// parameters name one shared resource must never be *running* at the same time.
///
/// Registration is what turns `ResMut<T>` into a resource id on the access
/// pattern (`SystemParam::report_access`), so this covers the id the scheduler is
/// actually handed rather than one a test wrote down. The counter makes an
/// overlap visible on its own: `ResMut::new` also takes the debug write lock, but
/// that only exists in debug builds, and it is keyed by the same id this test is
/// about - so in a release build the sleep either catches a parallel batch or
/// nothing does.
#[test]
fn two_systems_writing_one_shared_resource_never_overlap() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    // Both writers do the same three things with their own copy of the type:
    // note that they are running, hold long enough that a parallel batch cannot
    // hide, then write through the one shared slot.
    macro_rules! writer {
        ($resource:ty, $inside:expr, $overlaps:expr) => {{
            let inside = Arc::clone(&$inside);
            let overlaps = Arc::clone(&$overlaps);
            move |mut settings: ResMut<$resource>| {
                if inside.fetch_add(1, Ordering::SeqCst) != 0 {
                    overlaps.fetch_add(1, Ordering::SeqCst);
                }
                std::thread::sleep(Duration::from_millis(20));
                if let Some(mut settings) = settings.get_mut() {
                    settings.value += 1;
                }
                inside.fetch_sub(1, Ordering::SeqCst);
            }
        }};
    }

    let inside = Arc::new(AtomicUsize::new(0));
    let overlaps = Arc::new(AtomicUsize::new(0));

    let mut engine = Engine::new();
    engine
        .world_mut()
        .insert_resource(artifact_a::Settings { value: 0 });
    engine.register_system(
        "project_writer",
        writer!(artifact_a::Settings, inside, overlaps),
    );
    engine.register_system(
        "module_writer",
        writer!(artifact_b::Settings, inside, overlaps),
    );

    engine.process_frame().expect("the frame runs");

    assert_eq!(
        overlaps.load(Ordering::SeqCst),
        0,
        "the two writers of one shared resource overlapped, so they were batched together"
    );
    assert_eq!(
        engine
            .world()
            .get_resource::<artifact_a::Settings>()
            .expect("still inserted")
            .value,
        2,
        "both systems ran, and both writes landed on the one slot"
    );
}

// =============================================================================
// Guards
// =============================================================================

/// Two copies of one type is the case the feature exists for, and must not be
/// reported as a conflict.
#[test]
fn one_resource_type_compiled_twice_is_not_a_conflict() {
    let mut world = World::new();
    world.register_resource::<artifact_a::Settings>();
    world.register_resource::<artifact_b::Settings>();

    assert!(
        world.take_registration_error().is_none(),
        "two copies of one type must bind, not collide"
    );
}

/// Two *different* types claiming one shared name are reported, even though
/// their layouts agree exactly - the case no layout check can catch.
#[test]
fn two_types_claiming_one_shared_name_are_reported() {
    let mut world = World::new();
    world.register_resource::<artifact_a::Settings>();
    assert!(world.take_registration_error().is_none());

    world.register_resource::<UnrelatedSettings>();
    let error = world
        .take_registration_error()
        .expect("a second type claiming the name must be reported");

    match error {
        pill_engine::error::WorldError::SharedResourceNameConflict {
            shared_name,
            existing_type,
            incoming_type,
        } => {
            assert_eq!(shared_name, "demo::Settings");
            assert_eq!(existing_type, "Settings");
            assert_eq!(incoming_type, "UnrelatedSettings");
        }
        other => panic!("expected a shared-resource name conflict, got {other:?}"),
    }
}

/// One shared name with two memory shapes is reported rather than bound.
///
/// The type name matches here - both are `Settings` - so the name guard lets it
/// through and the layout guard is what has to catch it. That is the stale-
/// artifact case: one binary rebuilt against a changed definition.
#[test]
fn a_disagreeing_layout_under_one_shared_name_is_reported() {
    let mut world = World::new();
    world.register_resource::<artifact_a::Settings>();
    assert!(world.take_registration_error().is_none());

    world.register_resource::<stale_artifact::Settings>();
    let error = world
        .take_registration_error()
        .expect("a layout disagreement must be reported");

    match error {
        pill_engine::error::WorldError::SharedResourceLayoutMismatch {
            shared_name,
            existing_size,
            incoming_size,
            ..
        } => {
            assert_eq!(shared_name, "demo::Settings");
            assert_eq!(existing_size, std::mem::size_of::<artifact_a::Settings>());
            assert_eq!(
                incoming_size,
                std::mem::size_of::<stale_artifact::Settings>()
            );
        }
        other => panic!("expected a layout mismatch, got {other:?}"),
    }
}

/// Inserting - not just registering - runs the guards too, so a module that
/// only ever inserts is covered.
#[test]
fn inserting_also_runs_the_name_guard() {
    let mut world = World::new();
    world.insert_resource(artifact_a::Settings { value: 1 });
    assert!(world.take_registration_error().is_none());

    world.insert_resource(UnrelatedSettings { _value: 2 });
    assert!(
        world.take_registration_error().is_some(),
        "a conflicting claim must be reported even without register_resource"
    );
}

/// A refused claim must not touch what is stored.
///
/// The generation that made the claim is already failing - the drain turns the
/// recorded error into a failed init - so the only open question is whether the
/// live value survives it. `UnrelatedSettings` agrees with
/// `artifact_a::Settings` on size and alignment, so a reader would take the
/// wrong bytes without noticing: the size check that `holds` runs cannot
/// separate these two, and the guard that can has just been refused.
#[test]
fn a_refused_claim_leaves_the_stored_value_alone() {
    let mut world = World::new();
    world.insert_resource(artifact_a::Settings { value: 1 });
    assert!(world.take_registration_error().is_none());

    world.insert_resource(UnrelatedSettings { _value: 99 });
    assert!(
        world.take_registration_error().is_some(),
        "the name guard has to fire"
    );

    assert_eq!(
        world
            .get_resource::<artifact_a::Settings>()
            .map(|it| it.value),
        Some(1),
        "the refused generation's bytes must not reach the stored value"
    );
}

/// The same store, walked through the sequence a reload rolls back through.
///
/// The failed generation inserts under a shared name whose layout it disagrees
/// with, the rollback generation re-registers the resource, and the transaction
/// re-homes what survived. Nothing puts the old value back on that path - the
/// rollback only declares the resource - so a stored refused insert would leave
/// the box holding the failed shape under the rollback type's table: claimed,
/// registered, and unreadable, with a drop glue that no longer matches the
/// allocation it is about to run over.
#[test]
fn a_shape_change_cannot_strand_a_resource_across_a_rolled_back_reload() {
    let mut world = World::new();
    // The generation that owns the resource declares it, and a value is live.
    world.register_resource::<artifact_a::Settings>();
    world.insert_resource(artifact_a::Settings { value: 7 });
    assert!(world.take_registration_error().is_none());

    // A rebuild against a changed definition inserts under the same name.
    world.insert_resource(stale_artifact::Settings { value: 7, extra: 1 });
    assert!(
        world.take_registration_error().is_some(),
        "the layout guard has to fire"
    );

    // What the transaction does next: the failed generation is discarded, the
    // previous one initialises again, every surviving value is re-homed.
    world.register_resource::<artifact_a::Settings>();
    world.rehome_resources();

    assert_eq!(
        world
            .get_resource::<artifact_a::Settings>()
            .map(|it| it.value),
        Some(7),
        "the value has to survive a rolled-back reload"
    );
}

// =============================================================================
// Ordinary resources are unaffected
// =============================================================================

/// A resource that declares nothing keeps the strict per-artifact identity and
/// the ordinary behaviour, so shared identity is genuinely opt-in.
#[test]
fn ordinary_resources_are_unaffected() {
    let mut world = World::new();
    world.insert_resource(PlainCounter { value: 3 });
    world.insert_resource(OtherCounter::default());

    assert!(world.take_registration_error().is_none());
    assert_eq!(world.get_resource::<PlainCounter>().unwrap().value, 3);
    assert!(world.has_resource::<OtherCounter>());

    // Two ordinary resources are two slots.
    assert_eq!(world.resource_count(), 2);

    let taken = world
        .remove_resource::<PlainCounter>()
        .expect("the removal is not refused")
        .expect("removable");
    assert_eq!(taken.value, 3);
    assert_eq!(world.resource_count(), 1);
}
