//! Integration tests for components that keep one identity across binaries.
//!
//! # What is being simulated
//!
//! The situation these tests exist for cannot be reproduced literally in one
//! test binary: it needs one component type compiled into two artifacts loaded
//! into the same process, which is what gives that single type two `TypeId`s.
//!
//! Two distinct Rust types declaring the *same* `#[pill(shared = "...")]` name
//! reproduce it faithfully, because that is exactly what the engine sees in the
//! real case - two `TypeId`s, one declared identity, identical layouts - and it
//! is the only thing the engine ever sees of it. `ProjectSpline` stands for the
//! project's copy of the type and `ModuleSpline` for the module DLL's copy;
//! everything below is written from that reading.

use pill_engine::component::ComponentRegistry;
use pill_engine::query::{Changed, Query};
use pill_engine::{ComponentId, PillComponent, World};

/// The shared name both stand-in types declare, spelled once so a test can
/// assert against the identity it produces.
const SHARED_NAME: &str = "pill_spline::Spline";

// The two copies live in modules that stand for the two artifacts, and share
// the type's own name - exactly as the real case does, where `Spline` is
// `Spline` in whichever binary compiled it. Only the module path differs, which
// is the one thing an in-process simulation cannot reproduce faithfully.

/// The project's copy of the shared component.
pub mod project_copy {
    /// `pill_spline::Spline` as the project compiled it.
    #[derive(Clone, Debug, Default, pill_engine::PillComponent)]
    #[pill(shared = "pill_spline::Spline")]
    #[repr(C)]
    pub struct Spline {
        pub tension: f32,
        pub segments: u32,
    }
}

/// The module DLL's copy of the same component: a different Rust type, hence a
/// different `TypeId`, declaring the same shared name and the same layout.
pub mod module_copy {
    /// `pill_spline::Spline` as the module DLL compiled it.
    #[derive(Clone, Debug, Default, pill_engine::PillComponent)]
    #[pill(shared = "pill_spline::Spline")]
    #[repr(C)]
    pub struct Spline {
        pub tension: f32,
        pub segments: u32,
    }
}

use module_copy::Spline as ModuleSpline;
use project_copy::Spline as ProjectSpline;

/// An ordinary component, to show the default identity is untouched.
#[derive(Clone, Debug, Default, PillComponent)]
struct PlainMarker {
    value: u32,
}

/// A second ordinary component used to force an archetype that differs.
#[derive(Clone, Debug, Default, PillComponent)]
struct OtherMarker {
    value: u32,
}

// =============================================================================
// Identity
// =============================================================================

/// Both copies of the type resolve to the same [`ComponentId`], derived from
/// the declared name rather than from either binary's `TypeId`.
#[test]
fn both_copies_of_a_shared_type_resolve_to_one_component_id() {
    let project_id = ComponentId::of::<ProjectSpline>();
    let module_id = ComponentId::of::<ModuleSpline>();

    assert_eq!(
        project_id, module_id,
        "the two copies must be one component"
    );
    assert_eq!(
        project_id,
        ComponentId::Shared(pill_engine::component::shared_component_identity(
            SHARED_NAME
        )),
        "the identity is a function of the declared name alone"
    );
    // A component that declares nothing keeps the per-binary identity.
    assert!(matches!(
        ComponentId::of::<PlainMarker>(),
        ComponentId::Native(_)
    ));
}

/// The declared name is what the registry records, so every name-keyed lookup
/// finds the component under the name both binaries agreed on.
#[test]
fn a_shared_component_is_registered_under_its_declared_name() {
    let mut world = World::new();
    world.register_component::<ProjectSpline>();

    let component_id = ComponentId::of::<ProjectSpline>();
    assert_eq!(
        world.component_registry().get_name(&component_id),
        Some(SHARED_NAME)
    );
    assert_eq!(
        world.resolve_component_id_by_name_any(SHARED_NAME).unwrap(),
        Some(component_id)
    );
    assert_eq!(
        ComponentRegistry::registered_name::<ModuleSpline>(),
        SHARED_NAME,
        "the other binary's copy registers under the same name"
    );
}

// =============================================================================
// Binding: one bit, one column
// =============================================================================

/// The second registration binds to the column the first created rather than
/// allocating a second one: one bit is consumed, not two.
#[test]
fn the_second_registration_binds_instead_of_allocating() {
    let mut world = World::new();

    world.register_component::<ProjectSpline>();
    let after_first = world.component_registry().len();
    let bit = world
        .component_registry()
        .get_bit(&ComponentId::of::<ProjectSpline>())
        .expect("the first registration creates the bit");

    // The module's copy registers into the same world, as it would when its
    // DLL's `init` runs against the host's world.
    world.register_component::<ModuleSpline>();

    assert_eq!(
        world.component_registry().len(),
        after_first,
        "binding must not add a second registry entry"
    );
    assert_eq!(
        world
            .component_registry()
            .get_bit(&ComponentId::of::<ModuleSpline>()),
        Some(bit),
        "both copies must resolve to one mask bit"
    );
    assert!(
        world.take_registration_error().is_none(),
        "binding two matching layouts is not an error"
    );
}

/// Entities spawned through either copy land in the same archetype, which is
/// the outcome the whole feature exists to produce.
#[test]
fn entities_spawned_from_either_copy_share_one_archetype() {
    let mut world = World::new();
    world.register_component::<ProjectSpline>();
    world.register_component::<ModuleSpline>();

    let from_project = world
        .create_entity()
        .with(ProjectSpline {
            tension: 1.0,
            segments: 4,
        })
        .build()
        .unwrap();
    let from_module = world
        .create_entity()
        .with(ModuleSpline {
            tension: 2.0,
            segments: 8,
        })
        .build()
        .unwrap();

    let archetypes: Vec<_> = world
        .archetypes_iter()
        .filter(|archetype| !archetype.entities.is_empty())
        .collect();
    assert_eq!(
        archetypes.len(),
        1,
        "two copies of one component must not produce two archetypes"
    );
    assert_eq!(archetypes[0].entities.len(), 2);
    assert!(archetypes[0]
        .entities
        .contains(&from_project));
    assert!(archetypes[0].entities.contains(&from_module));

    // And both rows live in one column.
    assert_eq!(
        world.live_row_count(ComponentId::of::<ProjectSpline>()),
        2,
        "both entities' rows belong to the same component"
    );
}

// =============================================================================
// Access across the boundary
// =============================================================================

/// The decisive test: one copy writes through an ordinary typed query and the
/// other reads the result, because both address the same rows.
#[test]
fn a_write_through_one_copy_is_observed_through_the_other() {
    let mut world = World::new();
    world.register_component::<ProjectSpline>();
    world.register_component::<ModuleSpline>();

    let entity = world
        .create_entity()
        .with(ProjectSpline {
            tension: 1.0,
            segments: 4,
        })
        .build()
        .unwrap();

    // The module's systems query the component under its own type.
    {
        let mut query = Query::<(&mut ModuleSpline,)>::new(&mut world);
        let mut rows = 0;
        for (mut spline,) in query.iter_mut() {
            spline.tension = 9.5;
            spline.segments = 16;
            rows += 1;
        }
        assert_eq!(rows, 1, "the module's query must see the project's entity");
    }

    // The project reads its own type back and sees the module's writes.
    let observed = world
        .get_component::<ProjectSpline>(entity)
        .expect("the project still reaches the row through its own type");
    assert_eq!(observed.tension, 9.5);
    assert_eq!(observed.segments, 16);
}

/// Reading through the other copy works for plain `&T` queries too, and yields
/// every row regardless of which copy created it.
#[test]
fn a_read_query_through_one_copy_sees_rows_created_by_the_other() {
    let mut world = World::new();
    world.register_component::<ProjectSpline>();
    world.register_component::<ModuleSpline>();

    world
        .create_entity()
        .with(ProjectSpline {
            tension: 1.0,
            segments: 1,
        })
        .build()
        .unwrap();
    world
        .create_entity()
        .with(ModuleSpline {
            tension: 2.0,
            segments: 2,
        })
        .build()
        .unwrap();

    let mut query = Query::<(&ProjectSpline,)>::new(&mut world);
    let mut tensions: Vec<f32> = query
        .iter_mut()
        .map(|(spline,)| spline.tension)
        .collect();
    tensions.sort_by(f32::total_cmp);
    assert_eq!(tensions, vec![1.0, 2.0]);
}

/// A change tick written through one copy is seen by a `Changed` filter on the
/// other, so change detection does not fragment along the binary boundary.
#[test]
fn a_change_tick_written_by_one_copy_is_seen_by_the_other() {
    let mut world = World::new();
    world.register_component::<ProjectSpline>();
    world.register_component::<ModuleSpline>();

    world
        .create_entity()
        .with(ProjectSpline {
            tension: 1.0,
            segments: 1,
        })
        .build()
        .unwrap();

    // Move the world past the spawn so the row is not merely "just added".
    world.increment_change_tick();
    world.set_system_last_run(world.change_tick());
    world.increment_change_tick();

    // The module mutates the row.
    {
        let mut query = Query::<(&mut ModuleSpline,)>::new(&mut world);
        for (mut spline,) in query.iter_mut() {
            spline.tension = 4.0;
        }
    }

    // The project's `Changed` filter, keyed on its own type, observes it.
    let mut query =
        Query::<(&ProjectSpline,), Changed<ProjectSpline>>::new(&mut world);
    assert_eq!(
        query.iter_mut().count(),
        1,
        "the mutation must be visible to the other copy's change filter"
    );
}

// =============================================================================
// Scheduler soundness
// =============================================================================

/// Two systems writing the same shared component through different copies must
/// report a conflict, or the scheduler would batch them in parallel and they
/// would race on the same rows.
///
/// This is a soundness property rather than a correctness one, and it is why
/// binding and mask-sharing cannot be separated: both copies resolve to one
/// bit at the moment they resolve to one column, never later.
#[test]
fn two_systems_writing_through_different_copies_conflict() {
    use pill_engine::scheduler::SystemAccess;

    let mut world = World::new();
    world.register_component::<ProjectSpline>();
    world.register_component::<ModuleSpline>();
    let registry = world.component_registry();

    // The project's system writes the component under its own type.
    let mut project_access = SystemAccess::new();
    project_access.add_write(ComponentId::of::<ProjectSpline>());
    project_access.build_component_masks(registry);

    // The module's system writes it under its own type.
    let mut module_access = SystemAccess::new();
    module_access.add_write(ComponentId::of::<ModuleSpline>());
    module_access.build_component_masks(registry);

    assert!(
        project_access.masks_are_complete() && module_access.masks_are_complete(),
        "both accesses resolved to registry bits, so the fast path applies"
    );
    assert!(
        project_access.conflicts_with(&module_access),
        "two writes to one column must never be batched in parallel"
    );

    // A write and a read of the same shared component conflict as well.
    let mut reader_access = SystemAccess::new();
    reader_access.add_read(ComponentId::of::<ModuleSpline>());
    reader_access.build_component_masks(registry);
    assert!(project_access.conflicts_with(&reader_access));

    // An unrelated component still does not conflict, so the check has not
    // simply become "everything conflicts".
    world.register_component::<PlainMarker>();
    let registry = world.component_registry();
    let mut unrelated_access = SystemAccess::new();
    unrelated_access.add_write(ComponentId::of::<PlainMarker>());
    unrelated_access.build_component_masks(registry);
    let mut project_access = SystemAccess::new();
    project_access.add_write(ComponentId::of::<ProjectSpline>());
    project_access.build_component_masks(registry);
    assert!(!project_access.conflicts_with(&unrelated_access));
}

// =============================================================================
// Layout verification
// =============================================================================

/// The layout check rejects a second copy whose memory shape disagrees, rather
/// than binding it and letting one binary misread the other's rows.
#[test]
fn a_disagreeing_layout_is_reported_instead_of_bound() {
    // A third declaration of the same shared name whose fields are wider. In
    // the real case this is a module rebuilt against a changed definition while
    // the project still holds the old one.
    mod stale_copy {
        /// `pill_spline::Spline` as a module rebuilt against a changed
        /// definition compiled it: same name, same declared identity, wider
        /// fields.
        #[derive(Clone, Debug, Default, pill_engine::PillComponent)]
        #[pill(shared = "pill_spline::Spline")]
        #[repr(C)]
        pub struct Spline {
            pub tension: f64,
            pub segments: u64,
        }
    }
    use stale_copy::Spline as StaleSpline;

    let mut world = World::new();
    world.register_component::<ProjectSpline>();
    assert!(world.take_registration_error().is_none());

    world.register_component::<StaleSpline>();
    let error = world
        .take_registration_error()
        .expect("a layout disagreement must be reported");

    match error {
        pill_engine::error::WorldError::SharedComponentLayoutMismatch {
            shared_name,
            existing_size,
            incoming_size,
            ..
        } => {
            assert_eq!(shared_name, SHARED_NAME);
            assert_eq!(existing_size, std::mem::size_of::<ProjectSpline>());
            assert_eq!(incoming_size, std::mem::size_of::<StaleSpline>());
        }
        other => panic!("expected a layout mismatch, got {other:?}"),
    }
}

/// Size and alignment alone are not layout. Two shapes that agree on both but
/// differ in field types must still be rejected, because binding them would
/// reinterpret every row's bytes silently.
#[test]
fn a_same_sized_but_differently_typed_layout_is_still_rejected() {
    use pill_engine::component::component_schema_hash;
    use pill_engine::component_registry::ComponentFieldDescriptor;

    const AS_FLOATS: &[ComponentFieldDescriptor] = &[
        ComponentFieldDescriptor {
            name: "tension",
            type_tag: "f32",
            offset: 0,
            size: 4,
            align: 4,
            element_count: 0,
        },
        ComponentFieldDescriptor {
            name: "segments",
            type_tag: "f32",
            offset: 4,
            size: 4,
            align: 4,
            element_count: 0,
        },
    ];
    const AS_INTEGERS: &[ComponentFieldDescriptor] = &[
        ComponentFieldDescriptor {
            name: "tension",
            type_tag: "f32",
            offset: 0,
            size: 4,
            align: 4,
            element_count: 0,
        },
        ComponentFieldDescriptor {
            name: "segments",
            type_tag: "u32",
            offset: 4,
            size: 4,
            align: 4,
            element_count: 0,
        },
    ];

    assert_ne!(
        component_schema_hash(AS_FLOATS),
        component_schema_hash(AS_INTEGERS),
        "a field's type must change the schema hash even when its size does not"
    );

    // Reordering fields changes it too, though the struct's size does not.
    const REORDERED: &[ComponentFieldDescriptor] = &[
        ComponentFieldDescriptor {
            name: "segments",
            type_tag: "u32",
            offset: 0,
            size: 4,
            align: 4,
            element_count: 0,
        },
        ComponentFieldDescriptor {
            name: "tension",
            type_tag: "f32",
            offset: 4,
            size: 4,
            align: 4,
            element_count: 0,
        },
    ];
    assert_ne!(
        component_schema_hash(AS_INTEGERS),
        component_schema_hash(REORDERED),
        "a field reorder must change the schema hash"
    );
}

// =============================================================================
// Ordinary components are unaffected
// =============================================================================

/// Components that declare nothing keep entirely separate identities, columns
/// and archetypes, so shared identity is genuinely opt-in.
#[test]
fn ordinary_components_keep_separate_identities() {
    let mut world = World::new();
    world.register_component::<PlainMarker>();
    world.register_component::<OtherMarker>();

    assert_ne!(
        ComponentId::of::<PlainMarker>(),
        ComponentId::of::<OtherMarker>()
    );
    assert_eq!(world.component_registry().len(), 2, "two bits, not one");

    world
        .create_entity()
        .with(PlainMarker { value: 1 })
        .build()
        .unwrap();
    world
        .create_entity()
        .with(OtherMarker { value: 2 })
        .build()
        .unwrap();

    let populated = world
        .archetypes_iter()
        .filter(|archetype| !archetype.entities.is_empty())
        .count();
    assert_eq!(populated, 2, "unrelated components stay in their own columns");
}

// =============================================================================
// Paths that move rows rather than read them
// =============================================================================

/// Moving an entity between archetypes copies its shared component's row
/// through the registered copier, which belongs to whichever copy registered
/// last. That is only sound because both copies describe one type, so this
/// pins that a row written by one survives a move driven by the other.
#[test]
fn a_shared_row_survives_an_archetype_move() {
    let mut world = World::new();
    world.register_component::<ProjectSpline>();
    world.register_component::<ModuleSpline>();
    world.register_component::<PlainMarker>();

    let entity = world
        .create_entity()
        .with(ProjectSpline {
            tension: 3.25,
            segments: 12,
        })
        .build()
        .unwrap();

    // Adding a second component moves the entity into a different archetype,
    // copying every existing column's row across.
    world
        .add_component(entity, PlainMarker { value: 7 })
        .unwrap();

    let moved = world
        .get_component::<ProjectSpline>(entity)
        .expect("the row must survive the move");
    assert_eq!(moved.tension, 3.25);
    assert_eq!(moved.segments, 12);

    // And it is still the same single column, reachable from the other copy.
    let seen = world
        .get_component::<ModuleSpline>(entity)
        .expect("the other copy reaches the moved row too");
    assert_eq!(seen.tension, 3.25);
}

/// Removing a shared component drops its row through the column's registered
/// drop glue without disturbing the entity's other components.
#[test]
fn a_shared_row_can_be_removed_without_disturbing_its_neighbours() {
    let mut world = World::new();
    world.register_component::<ProjectSpline>();
    world.register_component::<ModuleSpline>();
    world.register_component::<PlainMarker>();

    let entity = world
        .create_entity()
        .with(ProjectSpline {
            tension: 1.5,
            segments: 3,
        })
        .with(PlainMarker { value: 11 })
        .build()
        .unwrap();

    // Removal is requested through the *other* copy's type.
    world.remove_component::<ModuleSpline>(entity).unwrap();

    assert!(world.get_component::<ProjectSpline>(entity).is_none());
    assert_eq!(world.get_component::<PlainMarker>(entity).unwrap().value, 11);
    assert_eq!(world.live_row_count(ComponentId::of::<ProjectSpline>()), 0);
}

// =============================================================================
// Hot reload
// =============================================================================

/// A persistable shared component, as each artifact compiled it.
pub mod project_persisted {
    /// `pill_spline::PersistedSpline` as the project compiled it.
    #[derive(
        Clone, Debug, Default, serde::Serialize, serde::Deserialize, pill_engine::PillComponent,
    )]
    #[pill(persistable, shared = "pill_spline::PersistedSpline")]
    #[repr(C)]
    pub struct PersistedSpline {
        pub tension: f32,
        pub segments: u32,
    }
}

/// The other binary's copy of the same persistable shared component.
pub mod module_persisted {
    /// `pill_spline::PersistedSpline` as the module DLL compiled it.
    #[derive(
        Clone, Debug, Default, serde::Serialize, serde::Deserialize, pill_engine::PillComponent,
    )]
    #[pill(persistable, shared = "pill_spline::PersistedSpline")]
    #[repr(C)]
    pub struct PersistedSpline {
        pub tension: f32,
        pub segments: u32,
    }
}

use module_persisted::PersistedSpline as ModulePersistedSpline;
use project_persisted::PersistedSpline as ProjectPersistedSpline;

/// Rows survive a snapshot and restore no matter which copy registered them.
///
/// This is the failure the whole plan started from. Before shared identity the
/// two copies were two components with the same type name, and registering the
/// second evicted the first's inserter from the persist maps - so at the next
/// reload the evicted copy's rows were dropped with no error at all. With one
/// identity there is only ever one registration to evict or keep.
#[test]
fn shared_rows_survive_a_snapshot_and_restore_from_either_copy() {
    let mut world = World::new();
    world.register_persistable_component::<ProjectPersistedSpline>();
    world.register_persistable_component::<ModulePersistedSpline>();
    assert!(
        world.take_registration_error().is_none(),
        "registering both copies must not report a peer collision"
    );

    // Rows created through each copy.
    world
        .create_entity()
        .with(ProjectPersistedSpline {
            tension: 1.0,
            segments: 1,
        })
        .build()
        .unwrap();
    world
        .create_entity()
        .with(ModulePersistedSpline {
            tension: 2.0,
            segments: 2,
        })
        .build()
        .unwrap();

    let snapshot = world.snapshot_components();
    assert_eq!(snapshot.entity_count(), 2);

    // A fresh world stands in for the world after a reload, with both copies
    // registering again exactly as their `init` functions would.
    let mut reloaded = World::new();
    reloaded.register_persistable_component::<ProjectPersistedSpline>();
    reloaded.register_persistable_component::<ModulePersistedSpline>();
    reloaded.restore_from_snapshot(&snapshot);

    let mut query = Query::<(&ProjectPersistedSpline,)>::new(&mut reloaded);
    let mut restored: Vec<(f32, u32)> = query
        .iter_mut()
        .map(|(spline,)| (spline.tension, spline.segments))
        .collect();
    restored.sort_by(|left, right| left.0.total_cmp(&right.0));

    assert_eq!(
        restored,
        vec![(1.0, 1), (2.0, 2)],
        "neither copy's rows may be dropped by the restore"
    );
}

/// The persistable manifest carries one entry for a shared component, under
/// its declared name, so the reload path sees one type rather than two rival
/// registrations of the same name.
#[test]
fn a_shared_persistable_component_appears_once_in_the_manifest() {
    let mut world = World::new();
    world.register_persistable_component::<ProjectPersistedSpline>();
    world.register_persistable_component::<ModulePersistedSpline>();

    let entries: Vec<_> = world
        .persist_type_manifest()
        .into_iter()
        .filter(|entry| entry.type_name == "pill_spline::PersistedSpline")
        .collect();

    assert_eq!(entries.len(), 1, "one component, one manifest entry");
    assert_eq!(
        entries[0].component_id,
        ComponentId::of::<ProjectPersistedSpline>()
    );
}

// =============================================================================
// Name collision between unrelated crates
// =============================================================================

// Two artifacts deliberately writing the *same qualified* name is the one
// route into a collision the derive cannot close - it is indistinguishable, at
// the macro, from the legitimate declaration two copies of one type make. The
// bare `"Transform"` these would otherwise use is now a compile error.

/// One crate's `Transform`.
#[derive(Clone, Debug, Default, PillComponent)]
#[pill(shared = "engine::Transform")]
#[repr(C)]
struct PhysicsTransform {
    x: f32,
    y: f32,
    z: f32,
}

/// An unrelated crate's `Transform` claiming the same qualified name, with a
/// *different* layout.
#[derive(Clone, Debug, Default, PillComponent)]
#[pill(shared = "engine::Transform")]
#[repr(C)]
struct UiTransform {
    x: f64,
    y: f64,
}

/// An unrelated crate's `Transform` claiming the same qualified name with a
/// byte-identical layout but a completely different meaning.
#[derive(Clone, Debug, Default, PillComponent)]
#[pill(shared = "engine::Transform")]
#[repr(C)]
struct AudioTransform {
    x: f32,
    y: f32,
    z: f32,
}

#[test]
fn colliding_names_with_different_layouts_are_caught() {
    let mut world = World::new();
    world.register_component::<PhysicsTransform>();
    assert!(world.take_registration_error().is_none());

    world.register_component::<UiTransform>();
    assert!(
        world.take_registration_error().is_some(),
        "a layout disagreement under one name must be reported"
    );
}

/// Two different types claiming one shared name are rejected even when their
/// layouts agree exactly - the case no layout check can catch, because every
/// read through either type would succeed and silently return the other
/// component's rows.
///
/// The discriminator is the Rust type's own name: two copies of one type agree
/// on it (`Spline` is `Spline` in whichever artifact compiled it), while two
/// unrelated components do not.
#[test]
fn identical_layouts_under_one_qualified_name_are_rejected() {
    let mut world = World::new();
    world.register_component::<PhysicsTransform>();
    assert!(world.take_registration_error().is_none());

    // Same declared name, byte-identical layout, entirely unrelated component.
    world.register_component::<AudioTransform>();
    let error = world
        .take_registration_error()
        .expect("two types claiming one shared name must be reported");

    match error {
        pill_engine::error::WorldError::SharedComponentNameConflict {
            shared_name,
            existing_type,
            incoming_type,
        } => {
            assert_eq!(shared_name, "engine::Transform");
            assert_eq!(existing_type, "PhysicsTransform");
            assert_eq!(incoming_type, "AudioTransform");
        }
        other => panic!("expected a shared-name conflict, got {other:?}"),
    }
}

/// The guard must not reject what the feature exists for: one type compiled
/// into two binaries. Those agree on the type's own name, which is exactly
/// what separates them from the collision above.
#[test]
fn one_type_compiled_twice_is_not_a_name_conflict() {
    let mut world = World::new();
    world.register_component::<ProjectSpline>();
    world.register_component::<ModuleSpline>();
    assert!(
        world.take_registration_error().is_none(),
        "two copies of one type must still bind"
    );
}
