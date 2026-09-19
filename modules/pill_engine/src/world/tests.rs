//! Unit tests for [`World`].
//!
//! # Responsibilities
//!
//! - Exercises the world's own behaviour: entity lifecycle and recycling,
//!   archetype migration, descriptor registration, relayout and remap, change
//!   ticks, and the diagnostics that read all of it back.
//!
//! # Design
//!
//! Kept beside the code they exercise rather than inside it: the code half of
//! this module is split by responsibility, and one test module that can reach
//! every one of them is what lets a test build a world out of whichever pieces
//! it needs without re-declaring the fixtures next door already have.

use super::*;
use crate::archetype::{FieldSource, LayoutField};

/// The change-detection ticks of one entity's row for one component.
fn ticks_of_row(world: &World, entity: Entity, component: ComponentId) -> ComponentTicks {
    let location = world.entity_locations[&entity];
    *world.archetypes[&location.archetype_id]
        .component_storages
        .get(component)
        .expect("the archetype owns the column")
        .row_ticks(location.index_in_archetype)
        .expect("the row exists")
}

/// Two u32 fields: the two-field shape the relayout tests start from.
fn two_u32_row(first: u32, second: u32) -> Vec<u8> {
    let mut bytes = first.to_ne_bytes().to_vec();
    bytes.extend_from_slice(&second.to_ne_bytes());
    bytes
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Position {
    x: f32,
    y: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Velocity {
    x: f32,
    y: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Health {
    hp: i32,
}

impl Component for Position {}
impl Component for Velocity {}
impl Component for Health {}

/// A resource used to pin registration-stamp behaviour.
#[derive(Debug, Default)]
struct AccountedResource {
    value: u32,
}
impl crate::resource::Resource for AccountedResource {}

/// A second one, so a fresh registration can be told from a replacement.
#[derive(Debug, Default)]
struct FreshAccountedResource {
    value: u32,
}
impl crate::resource::Resource for FreshAccountedResource {}

#[test]
fn descriptor_components_coexist_and_survive_archetype_migration() {
    let mut world = World::new();
    let a = world
        .register_component_descriptor(0xA1, "Project.A", 4, 4, 11, Blittability::engine_verified())
        .unwrap();
    let b = world
        .register_component_descriptor(0xB2, "Project.B", 4, 4, 22, Blittability::engine_verified())
        .unwrap();
    let c = world
        .register_component_descriptor(0xC3, "Project.C", 8, 8, 33, Blittability::engine_verified())
        .unwrap();
    let entity = world
        .create_descriptor_entity(&[
            (a, 10_u32.to_ne_bytes().to_vec()),
            (b, 20_u32.to_ne_bytes().to_vec()),
        ])
        .unwrap();

    assert_eq!(
        world.descriptor_component_bytes(entity, a).unwrap(),
        10_u32.to_ne_bytes()
    );
    assert_eq!(
        world.descriptor_component_bytes(entity, b).unwrap(),
        20_u32.to_ne_bytes()
    );

    world.add_descriptor_component_default(entity, c).unwrap();
    assert_eq!(
        world.descriptor_component_bytes(entity, a).unwrap(),
        10_u32.to_ne_bytes()
    );
    assert_eq!(
        world.descriptor_component_bytes(entity, b).unwrap(),
        20_u32.to_ne_bytes()
    );
    assert_eq!(world.descriptor_component_bytes(entity, c).unwrap(), [0; 8]);

    world.remove_descriptor_component(entity, b).unwrap();
    assert_eq!(
        world.descriptor_component_bytes(entity, a).unwrap(),
        10_u32.to_ne_bytes()
    );
    assert!(world.descriptor_component_bytes(entity, b).is_none());
    assert_eq!(world.descriptor_component_bytes(entity, c).unwrap(), [0; 8]);
}

/// A relayout rewrites rows where they are: entities keep their rows, values
/// follow their field names, and a field the old layout did not have starts
/// zeroed instead of holding a byte from the previous shape.
#[test]
fn relayout_migrates_rows_and_keeps_entities_in_their_rows() {
    let mut world = World::new();
    let component = world
        .register_component_descriptor(
            0xD4,
            "Project.Relayout",
            8,
            4,
            100,
            Blittability::engine_verified(),
        )
        .unwrap();
    let first = world
        .create_descriptor_entity(&[(component, two_u32_row(1, 2))])
        .unwrap();
    let second = world
        .create_descriptor_entity(&[(component, two_u32_row(3, 4))])
        .unwrap();

    let archetype = world.entity_locations[&first].archetype_id;
    let first_row = world.entity_locations[&first].index_in_archetype;
    let second_row = world.entity_locations[&second].index_in_archetype;
    assert_eq!(
        world.entity_locations[&second].archetype_id, archetype,
        "identical component sets share one archetype"
    );

    // `b` first, then `a`, then an eight-byte field that did not exist.
    let plan = FieldPlan::between(
        &[
            LayoutField {
                name: "a",
                type_tag: "u32",
                offset: 0,
                size: 4,
            },
            LayoutField {
                name: "b",
                type_tag: "u32",
                offset: 4,
                size: 4,
            },
        ],
        &[
            LayoutField {
                name: "b",
                type_tag: "u32",
                offset: 0,
                size: 4,
            },
            LayoutField {
                name: "a",
                type_tag: "u32",
                offset: 4,
                size: 4,
            },
            LayoutField {
                name: "added",
                type_tag: "u32",
                offset: 8,
                size: 8,
            },
        ],
    );

    let migrated = world
        .relayout_descriptor_component(component, 16, 8, 200, &plan)
        .unwrap();

    assert_eq!(migrated, 2);
    let mut expected_first = two_u32_row(2, 1);
    expected_first.extend_from_slice(&[0_u8; 8]);
    assert_eq!(
        world.descriptor_component_bytes(first, component).unwrap(),
        expected_first.as_slice()
    );
    let mut expected_second = two_u32_row(4, 3);
    expected_second.extend_from_slice(&[0_u8; 8]);
    assert_eq!(
        world.descriptor_component_bytes(second, component).unwrap(),
        expected_second.as_slice()
    );
    assert_eq!(
        world.entity_locations[&first].archetype_id, archetype,
        "a relayout moves no entity between archetypes"
    );
    assert_eq!(world.entity_locations[&first].index_in_archetype, first_row);
    assert_eq!(
        world.entity_locations[&second].index_in_archetype,
        second_row
    );
    assert_eq!(world.component_layout(component), Some((16, 8)));
}

/// A relayout is a structural edit, not an add: rows keep their ticks, so no
/// `Added` filter fires and a row that was already changed stays changed.
#[test]
fn relayout_leaves_change_ticks_alone() {
    let mut world = World::new();
    let component = world
        .register_component_descriptor(
            0xD5,
            "Project.Ticks",
            8,
            4,
            100,
            Blittability::engine_verified(),
        )
        .unwrap();
    let entity = world
        .create_descriptor_entity(&[(component, two_u32_row(1, 2))])
        .unwrap();

    let changed_tick = world.increment_change_tick();
    let location = world.entity_locations[&entity];
    world
        .archetypes
        .get_mut(&location.archetype_id)
        .unwrap()
        .component_storages
        .get_mut(component)
        .unwrap()
        .row_ticks_mut(location.index_in_archetype)
        .unwrap()
        .set_changed(changed_tick);
    let before = ticks_of_row(&world, entity, component);

    world
        .relayout_descriptor_component(component, 16, 8, 200, &FieldPlan::new())
        .unwrap();

    let after = ticks_of_row(&world, entity, component);
    assert_eq!(after.added, before.added);
    assert_eq!(after.changed, changed_tick);
}

/// A remap carries rows from the predecessor registration to the
/// successor: the values move, the successor answers to its own name, the
/// predecessor's registration is retired, and an entity whose only
/// component this is survives the move.
#[test]
fn remap_moves_rows_to_the_successor_and_retires_the_source() {
    let mut world = World::new();
    let old = world
        .register_component_descriptor(
            0xE5,
            "Project.OldName",
            8,
            4,
            200,
            Blittability::engine_verified(),
        )
        .unwrap();
    let new = world
        .register_component_descriptor(
            0xF6,
            "Project.NewName",
            8,
            4,
            200,
            Blittability::engine_verified(),
        )
        .unwrap();
    let companion = world
        .register_component_descriptor(
            0x1A,
            "Project.Companion",
            4,
            4,
            9,
            Blittability::engine_verified(),
        )
        .unwrap();
    // One entity carries only the component being moved: the
    // add-before-remove order is what keeps it alive.
    let alone = world
        .create_descriptor_entity(&[(old, two_u32_row(7, 8))])
        .unwrap();
    let paired = world
        .create_descriptor_entity(&[
            (old, two_u32_row(1, 2)),
            (companion, 3_u32.to_ne_bytes().to_vec()),
        ])
        .unwrap();

    let fields = vec![
        LayoutField {
            name: "a",
            type_tag: "u32",
            offset: 0,
            size: 4,
        },
        LayoutField {
            name: "b",
            type_tag: "u32",
            offset: 4,
            size: 4,
        },
    ];
    let plan = FieldPlan::between(&fields, &fields);
    let moved = world.remap_descriptor_component(old, new, &plan).unwrap();

    assert_eq!(moved, 2);
    assert_eq!(
        world.descriptor_component_bytes(alone, new).unwrap(),
        two_u32_row(7, 8)
    );
    assert_eq!(
        world.descriptor_component_bytes(paired, new).unwrap(),
        two_u32_row(1, 2)
    );
    assert_eq!(
        world.descriptor_component_bytes(paired, companion).unwrap(),
        3_u32.to_ne_bytes(),
        "the companion column survives the move"
    );
    assert!(world.descriptor_component_bytes(alone, old).is_none());
    assert!(
        !world.storage_factories.contains_key(&old),
        "the predecessor's registration is retired"
    );
    assert_eq!(
        world
            .resolve_component_id_by_name_any("Project.OldName")
            .unwrap(),
        None,
        "the old name is free again"
    );
    assert_eq!(
        world
            .resolve_component_id_by_name_any("Project.NewName")
            .unwrap(),
        Some(new)
    );
}

/// The plan's field mapping is what moves, not the bytes: a shape with a
/// moved field and an added one reorders and zero-fills, exactly as a
/// relayout would.
#[test]
fn remap_reshapes_rows_through_the_plan() {
    let mut world = World::new();
    let old = world
        .register_component_descriptor(
            0xE6,
            "Project.PlanOld",
            8,
            4,
            300,
            Blittability::engine_verified(),
        )
        .unwrap();
    let new = world
        .register_component_descriptor(
            0xF7,
            "Project.PlanNew",
            12,
            4,
            301,
            Blittability::engine_verified(),
        )
        .unwrap();
    let entity = world
        .create_descriptor_entity(&[(old, two_u32_row(1, 2))])
        .unwrap();

    let plan = FieldPlan::between(
        &[
            LayoutField {
                name: "a",
                type_tag: "u32",
                offset: 0,
                size: 4,
            },
            LayoutField {
                name: "b",
                type_tag: "u32",
                offset: 4,
                size: 4,
            },
        ],
        &[
            LayoutField {
                name: "b",
                type_tag: "u32",
                offset: 0,
                size: 4,
            },
            LayoutField {
                name: "a",
                type_tag: "u32",
                offset: 4,
                size: 4,
            },
            LayoutField {
                name: "added",
                type_tag: "u32",
                offset: 8,
                size: 4,
            },
        ],
    );
    world.remap_descriptor_component(old, new, &plan).unwrap();

    let mut expected = 2_u32.to_ne_bytes().to_vec();
    expected.extend_from_slice(&1_u32.to_ne_bytes());
    expected.extend_from_slice(&0_u32.to_ne_bytes());
    assert_eq!(
        world.descriptor_component_bytes(entity, new).unwrap(),
        expected.as_slice()
    );
}

/// A plan default rides the plan: an added field is filled with the
/// supplied bytes instead of zero, and setting one on a copied field is a
/// no-op rather than an overwrite.
#[test]
fn a_plan_default_fills_an_added_field_and_never_overrides_a_copied_one() {
    let mut world = World::new();
    let component = world
        .register_component_descriptor(
            0xD8,
            "Project.Defaults",
            8,
            4,
            100,
            Blittability::engine_verified(),
        )
        .unwrap();
    let entity = world
        .create_descriptor_entity(&[(component, two_u32_row(1, 2))])
        .unwrap();

    let mut plan = FieldPlan::between(
        &[
            LayoutField {
                name: "a",
                type_tag: "u32",
                offset: 0,
                size: 4,
            },
            LayoutField {
                name: "b",
                type_tag: "u32",
                offset: 4,
                size: 4,
            },
        ],
        &[
            LayoutField {
                name: "a",
                type_tag: "u32",
                offset: 0,
                size: 4,
            },
            LayoutField {
                name: "b",
                type_tag: "u32",
                offset: 4,
                size: 4,
            },
            LayoutField {
                name: "added",
                type_tag: "u32",
                offset: 8,
                size: 4,
            },
        ],
    );
    plan.set_default(8, &6_u32.to_ne_bytes())
        .expect("the added field has a planned slot");
    // A default on a copied field is accepted and ignored, so the carried
    // value survives even a default declared for it.
    plan.set_default(0, &9_u32.to_ne_bytes())
        .expect("setting on a copied field is accepted");

    world
        .relayout_descriptor_component(component, 12, 4, 101, &plan)
        .unwrap();

    let mut expected = 1_u32.to_ne_bytes().to_vec();
    expected.extend_from_slice(&2_u32.to_ne_bytes());
    expected.extend_from_slice(&6_u32.to_ne_bytes());
    assert_eq!(
        world.descriptor_component_bytes(entity, component).unwrap(),
        expected.as_slice(),
        "the added field takes its default and the copied fields keep their values"
    );
}

/// Retiring a descriptor component's storage removes its rows and its
/// registration, and frees the name and the bit for a later declaration -
/// the contract a managed manifest's removed type rides on.
#[test]
fn retire_component_storage_frees_a_descriptor_component() {
    let mut world = World::new();
    let component = world
        .register_component_descriptor(
            0xB9,
            "Project.Retired",
            4,
            4,
            400,
            Blittability::engine_verified(),
        )
        .unwrap();
    let companion = world
        .register_component_descriptor(
            0xCA,
            "Project.Stays",
            4,
            4,
            401,
            Blittability::engine_verified(),
        )
        .unwrap();
    let entity = world
        .create_descriptor_entity(&[
            (component, 5_u32.to_ne_bytes().to_vec()),
            (companion, 6_u32.to_ne_bytes().to_vec()),
        ])
        .unwrap();

    let affected = world.retire_component_storage(&[component]);

    assert_eq!(affected, 1, "the retired component's row went");
    assert!(world
        .descriptor_component_bytes(entity, component)
        .is_none());
    assert_eq!(
        world.descriptor_component_bytes(entity, companion).unwrap(),
        6_u32.to_ne_bytes(),
        "the companion column survives the retirement"
    );
    assert!(
        !world.storage_factories.contains_key(&component),
        "the registration is forgotten"
    );
    assert_eq!(
        world
            .resolve_component_id_by_name_any("Project.Retired")
            .unwrap(),
        None,
        "the name is free again"
    );

    // The name and the bit can be taken by a later declaration.
    let replacement = world
        .register_component_descriptor(
            0xCB,
            "Project.Retired",
            4,
            4,
            402,
            Blittability::engine_verified(),
        )
        .unwrap();
    assert_eq!(
        world
            .resolve_component_id_by_name_any("Project.Retired")
            .unwrap(),
        Some(replacement)
    );
}

/// The resource twin: the value moves onto the successor's id, claims
/// travel with it, and the source declaration is dropped once they have.
#[test]
fn remap_foreign_resource_moves_the_value_and_the_claim() {
    let mut world = World::new();
    let old = world
        .register_foreign_resource("Project.OldSettings", "Project.OldSettings", 8, 4, 77)
        .unwrap();
    world
        .insert_foreign_resource_bytes(old, &two_u32_row(5, 6))
        .unwrap();
    let new = world
        .register_foreign_resource("Project.NewSettings", "Project.NewSettings", 8, 4, 77)
        .unwrap();
    // The host seeds a fresh declaration with zeroes; the moved value
    // replaces that seed.
    world
        .insert_foreign_resource_bytes(new, &[0_u8; 8])
        .unwrap();
    world.retain_resource_claims(&[old]);

    let fields = vec![
        LayoutField {
            name: "a",
            type_tag: "u32",
            offset: 0,
            size: 4,
        },
        LayoutField {
            name: "b",
            type_tag: "u32",
            offset: 4,
            size: 4,
        },
    ];
    let plan = FieldPlan::between(&fields, &fields);
    let moved = world.remap_foreign_resource(old, new, &plan).unwrap();

    assert_eq!(moved, 1);
    assert_eq!(
        world.foreign_resource_bytes(new).unwrap(),
        two_u32_row(5, 6)
    );
    assert!(
        world.foreign_resource_layout(old).is_none(),
        "the source declaration is dropped once its claims moved"
    );
    assert_eq!(world.resource_claim_counts.get(&new).copied(), Some(1));
    assert!(!world.resource_claim_counts.contains_key(&old));
}

/// The id and the registry bit survive, because they are baked into
/// archetype masks and scheduled access masks: a relayout must update the
/// layout in place, never re-register the component.
#[test]
fn relayout_keeps_the_component_id_and_its_bit() {
    let mut world = World::new();
    let component = world
        .register_component_descriptor(
            0xD6,
            "Project.Bit",
            8,
            4,
            100,
            Blittability::engine_verified(),
        )
        .unwrap();
    let bit = world.component_registry().get_bit(&component).unwrap();

    world
        .relayout_descriptor_component(component, 16, 8, 200, &FieldPlan::new())
        .unwrap();

    // The same id still names the same component; only its size moved.
    assert_eq!(world.component_registry().get_bit(&component), Some(bit));
    assert_eq!(world.component_registry().get_size(&component), Some(16));
    assert_eq!(
        world.component_registry().get_name(&component),
        Some("Project.Bit")
    );
}

/// A relayout has to reach archetypes whose last row is gone: the next
/// entity added there must be stored at the new size. The push only
/// succeeds when the column there really was rewritten.
#[test]
fn relayout_reaches_an_archetype_that_lost_its_last_row() {
    let mut world = World::new();
    let component = world
        .register_component_descriptor(
            0xD7,
            "Project.Empty",
            8,
            4,
            100,
            Blittability::engine_verified(),
        )
        .unwrap();
    let disposable = world
        .create_descriptor_entity(&[(component, two_u32_row(1, 2))])
        .unwrap();
    let archetype = world.entity_locations[&disposable].archetype_id;
    assert!(world.destroy_entity(disposable));

    world
        .relayout_descriptor_component(component, 16, 8, 200, &FieldPlan::new())
        .unwrap();

    let entity = world
        .create_descriptor_entity(&[(component, vec![7_u8; 16])])
        .unwrap();
    assert_eq!(
        world.entity_locations[&entity].archetype_id, archetype,
        "the empty archetype is reused, so its column is the one being tested"
    );
    assert_eq!(
        world.descriptor_component_bytes(entity, component).unwrap(),
        [7_u8; 16].as_slice()
    );
}

/// Only a registered descriptor component has a layout to replace, and a plan
/// that does not fit both layouts is refused before anything is touched.
#[test]
fn relayout_refuses_what_it_cannot_migrate() {
    let mut world = World::new();
    world.register_component::<Position>();
    assert!(matches!(
        world.relayout_descriptor_component(
            ComponentId::of::<Position>(),
            16,
            8,
            1,
            &FieldPlan::new()
        ),
        Err(WorldError::DescriptorComponentNotRegistered { .. })
    ));
    assert!(matches!(
        world.relayout_descriptor_component(
            ComponentId::descriptor(0xEE),
            16,
            8,
            1,
            &FieldPlan::new()
        ),
        Err(WorldError::DescriptorComponentNotRegistered { .. })
    ));

    let component = world
        .register_component_descriptor(
            0xD8,
            "Project.Refused",
            8,
            4,
            100,
            Blittability::engine_verified(),
        )
        .unwrap();
    let mut too_wide = FieldPlan::new();
    too_wide.push(4, 8, FieldSource::OldOffset(0));
    assert!(matches!(
        world.relayout_descriptor_component(component, 8, 4, 100, &too_wide),
        Err(WorldError::DescriptorRowInvalid)
    ));
    assert_eq!(
        world.component_layout(component),
        Some((8, 4)),
        "a refused plan leaves the registered layout as it was"
    );

    // A column that goes missing is reported before the first migration,
    // so the archetypes holding rows keep them. The destination archetype
    // is made to list the component without storing a column for it.
    let holder = world
        .create_descriptor_entity(&[(component, 8_u64.to_ne_bytes().to_vec())])
        .unwrap();
    let stripped = world.get_or_create_archetype(vec![component, ComponentId::of::<Position>()]);
    assert!(world
        .archetypes
        .get_mut(&stripped)
        .unwrap()
        .component_storages
        .remove(component)
        .is_some());
    let row_before = world
        .descriptor_component_bytes(holder, component)
        .expect("the entity has a row")
        .to_vec();
    assert!(matches!(
        world.relayout_descriptor_component(component, 8, 4, 100, &FieldPlan::new()),
        Err(WorldError::DescriptorStorageMissing { .. })
    ));
    assert_eq!(
        world.descriptor_component_bytes(holder, component),
        Some(row_before.as_slice()),
        "a refused relayout left the row it would have migrated alone"
    );

    // Put the stripped column back, then drift one behind the factory's
    // back: the size mismatch is reported too, and the column keeps its
    // drifted shape instead of being silently rewritten.
    world
        .archetypes
        .get_mut(&stripped)
        .unwrap()
        .component_storages
        .insert(
            component,
            crate::archetype::ComponentColumn::new(ColumnLayout {
                size: 8,
                align: 4,
                schema_hash: 100,
                blittability: Blittability::engine_verified(),
            })
            .expect("the drifted layout is still a valid allocation layout"),
        );
    {
        let location = world.entity_locations[&holder];
        let column = world
            .archetypes
            .get_mut(&location.archetype_id)
            .unwrap()
            .component_storages
            .get_mut(component)
            .unwrap();
        column.relayout_validated(
            ColumnLayout {
                size: 16,
                align: 4,
                schema_hash: 100,
                blittability: Blittability::engine_verified(),
            },
            &FieldPlan::new(),
            8,
        );
    }
    assert!(matches!(
        world.relayout_descriptor_component(component, 8, 4, 100, &FieldPlan::new()),
        Err(WorldError::ComponentColumnLayoutMismatch { .. })
    ));
    assert_eq!(
        world.archetypes[&world.entity_locations[&holder].archetype_id]
            .component_storages
            .get(component)
            .expect("column")
            .element_size(),
        16,
        "the mismatched column was reported, not rewritten"
    );
    assert_eq!(
        world.component_layout(component),
        Some((8, 4)),
        "the factory still carries the registered layout"
    );
}

/// A failed migration is atomic: the pre-flight refuses the destination
/// before the entity's row, its columns or its location move anywhere.
#[test]
fn migration_failure_leaves_both_archetypes_untouched() {
    let mut world = World::new();
    world.register_component::<Position>();
    let first = world
        .register_component_descriptor(
            0xE1,
            "Project.First",
            4,
            4,
            1,
            Blittability::engine_verified(),
        )
        .unwrap();
    let second = world
        .register_component_descriptor(
            0xE2,
            "Project.Second",
            4,
            4,
            2,
            Blittability::engine_verified(),
        )
        .unwrap();

    // Make the {first, second} destination exist, then manufacture the
    // desync: it lists `second` without storing a column for it.
    let destination = world.get_or_create_archetype(vec![first, second]);
    assert!(world
        .archetypes
        .get_mut(&destination)
        .unwrap()
        .component_storages
        .remove(second)
        .is_some());

    let entity = world
        .create_descriptor_entity(&[(first, 7_u32.to_ne_bytes().to_vec())])
        .unwrap();
    let source = world.entity_locations[&entity].archetype_id;
    let source_rows = world.archetypes[&source].entities.len();
    let source_column = world.archetypes[&source]
        .component_storages
        .get(first)
        .expect("column")
        .len();
    let destination_rows = world.archetypes[&destination].entities.len();

    assert!(matches!(
        world.add_descriptor_component_default(entity, second),
        Err(WorldError::DescriptorStorageMissing { .. })
    ));

    assert_eq!(world.entity_locations[&entity].archetype_id, source);
    assert_eq!(
        world.archetypes[&source].entities.len(),
        source_rows,
        "the source kept its row while the destination refused the migration"
    );
    assert_eq!(
        world.archetypes[&source]
            .component_storages
            .get(first)
            .expect("column")
            .len(),
        source_column
    );
    assert_eq!(
        world.archetypes[&destination].entities.len(),
        destination_rows,
        "the destination never received the entity"
    );
}

#[test]
fn descriptor_component_ticks_survive_archetype_migration() {
    fn ticks_for(world: &World, entity: Entity, component: ComponentId) -> ComponentTicks {
        let location = world.entity_locations[&entity];
        *world.archetypes[&location.archetype_id]
            .component_storages
            .get(component)
            .expect("the archetype owns the column")
            .row_ticks(location.index_in_archetype)
            .expect("the row exists")
    }

    let mut world = World::new();
    let retained = world
        .register_component_descriptor(
            0xA1,
            "Project.Retained",
            4,
            4,
            11,
            Blittability::engine_verified(),
        )
        .unwrap();
    let removed = world
        .register_component_descriptor(
            0xB2,
            "Project.Removed",
            4,
            4,
            22,
            Blittability::engine_verified(),
        )
        .unwrap();
    let added = world
        .register_component_descriptor(
            0xC3,
            "Project.Added",
            8,
            8,
            33,
            Blittability::engine_verified(),
        )
        .unwrap();
    let entity = world
        .create_descriptor_entity(&[
            (retained, 10_u32.to_ne_bytes().to_vec()),
            (removed, 20_u32.to_ne_bytes().to_vec()),
        ])
        .unwrap();

    let original_retained_ticks = ticks_for(&world, entity, retained);
    let original_removed_ticks = ticks_for(&world, entity, removed);
    let changed_tick = world.increment_change_tick();
    let location = world.entity_locations[&entity];
    world
        .archetypes
        .get_mut(&location.archetype_id)
        .unwrap()
        .component_storages
        .get_mut(retained)
        .unwrap()
        .row_ticks_mut(location.index_in_archetype)
        .unwrap()
        .set_changed(changed_tick);
    let retained_before_migration = ticks_for(&world, entity, retained);
    assert_eq!(
        retained_before_migration.added,
        original_retained_ticks.added
    );
    assert_eq!(retained_before_migration.changed, changed_tick);

    let addition_tick = world.increment_change_tick();
    world
        .add_descriptor_component_default(entity, added)
        .unwrap();

    let retained_after_add = ticks_for(&world, entity, retained);
    let removed_after_add = ticks_for(&world, entity, removed);
    let added_after_add = ticks_for(&world, entity, added);
    assert_eq!(retained_after_add.added, retained_before_migration.added);
    assert_eq!(
        retained_after_add.changed,
        retained_before_migration.changed
    );
    assert_eq!(removed_after_add.added, original_removed_ticks.added);
    assert_eq!(removed_after_add.changed, original_removed_ticks.changed);
    assert_eq!(added_after_add.added, addition_tick);
    assert_eq!(added_after_add.changed, addition_tick);

    world.increment_change_tick();
    world.remove_descriptor_component(entity, removed).unwrap();

    let retained_after_remove = ticks_for(&world, entity, retained);
    let added_after_remove = ticks_for(&world, entity, added);
    assert_eq!(retained_after_remove.added, retained_after_add.added);
    assert_eq!(retained_after_remove.changed, retained_after_add.changed);
    assert_eq!(added_after_remove.added, added_after_add.added);
    assert_eq!(added_after_remove.changed, added_after_add.changed);
    assert!(world.descriptor_component_bytes(entity, removed).is_none());
}

/// The native byte-chunk accessor exposes a native column's rows as raw
/// bytes with the correct element size, mirroring the descriptor path used
/// by the C# backend for optional-module components. Descriptor components
/// are rejected by it.
#[test]
fn native_component_chunk_mut_exposes_raw_rows() {
    let mut world = World::new();
    world.register_component::<Position>();
    let entity = world
        .create_entity()
        .with(Position { x: 1.0, y: 2.0 })
        .build()
        .unwrap();
    let component_id = ComponentId::of::<Position>();
    // Copy the raw facts out while the mutable chunk borrow is still
    // scoped, so the world can be re-borrowed below.
    let (archetype_id, data, len, element_size, ticks_len) = {
        let (archetype_id, data, len, element_size, ticks) = world
            .native_component_chunk_mut(component_id, 0)
            .expect("one archetype column should exist");
        (archetype_id, data, len, element_size, ticks.len())
    };
    assert_eq!(archetype_id, world.entity_locations[&entity].archetype_id);
    assert_eq!(len, 1);
    assert_eq!(element_size, std::mem::size_of::<Position>());
    assert_eq!(ticks_len, 1);
    // SAFETY: `len` is 1 and `element_size` is the Position size, so the
    // returned buffer holds exactly one valid Position.
    let row = unsafe { &*data.cast::<Position>() };
    assert_eq!(row.x, 1.0);
    assert_eq!(row.y, 2.0);

    // Descriptor components are served by the descriptor accessor, not this one.
    let descriptor = world
        .register_component_descriptor(
            0xD1,
            "NativeChunkTest.Descriptor",
            4,
            4,
            1,
            Blittability::engine_verified(),
        )
        .unwrap();
    assert!(world.native_component_chunk_mut(descriptor, 0).is_none());
    // An unknown native id is rejected too.
    let unknown = world
        .register_component_descriptor(
            0xD2,
            "NativeChunkTest.Unknown",
            4,
            4,
            2,
            Blittability::engine_verified(),
        )
        .unwrap();
    assert!(world.native_component_chunk_mut(unknown, 0).is_none());
}

/// A byte component adder writes raw ABI bytes into a native column, which
/// is how the C# backend creates or adds optional-module components whose
/// concrete Rust type the host never names.
#[test]
fn byte_component_adder_writes_native_bytes() {
    use crate::commands::{ByteComponentAdder, CommandQueue, ComponentAdder};

    let mut world = World::new();
    world.register_component::<Position>();
    let entity = world.reserve_entity();
    let component_id = ComponentId::of::<Position>();

    // ABI payload: x = 7.5, y = -3.25, little-endian f32s.
    let mut bytes = Vec::with_capacity(std::mem::size_of::<Position>());
    bytes.extend_from_slice(&7.5_f32.to_ne_bytes());
    bytes.extend_from_slice(&(-3.25_f32).to_ne_bytes());

    let adder = ByteComponentAdder::new(component_id, bytes);
    let mut queue = CommandQueue::new();
    queue.create_mixed_entity(
        entity,
        vec![Box::new(adder) as Box<dyn ComponentAdder>],
        Vec::new(),
    );
    queue.execute_queued_commands(&mut world, true).unwrap();

    let position = world
        .get_component::<Position>(entity)
        .expect("row was created");
    assert_eq!(position.x, 7.5);
    assert_eq!(position.y, -3.25);
}

/// The id-keyed sweep releases a column whose factory was purged, empty
/// or not: the archetype stops listing the id and no column without a
/// table maker is left behind for a graveyard eviction to invalidate.
#[test]
fn a_column_whose_factory_vanished_is_dropped_before_eviction() {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    let component_id = ComponentId::of::<Position>();
    // Two components, so the entity survives the sweep and its destination
    // archetype can be inspected.
    let entity = world
        .create_entity()
        .with(Position { x: 1.0, y: 2.0 })
        .with(Velocity { x: 3.0, y: 4.0 })
        .build()
        .unwrap();

    // What `forget_component_type` leaves when a rebuilt image re-registers
    // the name under a fresh id: the archetype keeps the id and its column,
    // while the factory that describes them is gone.
    world.storage_factories.remove(&component_id);
    assert_eq!(
        world.columns_without_factory(),
        1,
        "the column is the only one without a factory"
    );

    let dropped = world.drop_columns_without_factory();
    assert_eq!(dropped, 1, "the orphaned id was the one swept");
    assert_eq!(
        world.columns_without_factory(),
        0,
        "no column without a function table survives the sweep"
    );
    let location = world.entity_locations[&entity];
    assert!(
        !world.archetypes[&location.archetype_id]
            .component_types
            .contains(&component_id),
        "the entity no longer lists the swept component"
    );
    assert_eq!(
        world.get_component::<Velocity>(entity).unwrap().x,
        3.0,
        "the surviving component kept its value through the migration"
    );
}

#[test]
fn invalid_descriptor_component_layouts_are_rejected() {
    let mut world = World::new();
    assert!(world
        .register_component_descriptor(1, "Zero", 0, 1, 0, Blittability::engine_verified())
        .is_err());
    assert!(world
        .register_component_descriptor(2, "BadAlign", 4, 3, 0, Blittability::engine_verified())
        .is_err());
    assert!(world
        .register_component_descriptor(
            4,
            "Oversized",
            usize::MAX,
            1,
            0,
            Blittability::engine_verified()
        )
        .is_err());
    world
        .register_component_descriptor(3, "SchemaA", 4, 4, 10, Blittability::engine_verified())
        .unwrap();
    assert!(world
        .register_component_descriptor(
            3,
            "SameSchemaDifferentName",
            4,
            4,
            10,
            Blittability::engine_verified()
        )
        .is_err());
    assert!(world
        .register_component_descriptor(3, "SchemaB", 8, 8, 20, Blittability::engine_verified())
        .is_err());

    let valid = world
        .register_component_descriptor(5, "Valid", 4, 4, 30, Blittability::engine_verified())
        .unwrap();
    assert!(world
        .create_descriptor_entity(&[(valid, vec![0; 3])])
        .is_err());
    assert_eq!(world.entity_count(), 0);
}

/// Tests creating multiple entities with different component combinations.
///
/// This test verifies that:
/// - Entities can be created with various combinations of components
/// - Each unique component combination creates a separate archetype
/// - All created entities are properly tracked in the world
///
/// Expected results:
/// - 3 entities should be created in total
/// - 3 different archetypes should exist (Position+Velocity, Position, Position+Velocity+Health)
/// - All entity IDs should be present in the entity_locations map
#[test]
fn test_create_entities_with_different_components() {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();

    // Create entity with Position + Velocity
    let entity1 = world
        .create_entity()
        .with(Position { x: 10.0, y: 20.0 })
        .with(Velocity { x: 1.0, y: 2.0 })
        .build()
        .unwrap();

    // Create entity with Position only
    let entity2 = world
        .create_entity()
        .with(Position { x: 5.0, y: 15.0 })
        .build()
        .unwrap();

    // Create entity with all three components
    let entity3 = world
        .create_entity()
        .with(Position { x: 100.0, y: 200.0 })
        .with(Velocity { x: 5.0, y: 10.0 })
        .with(Health { hp: 100 })
        .build()
        .unwrap();

    assert_eq!(world.entity_locations.len(), 3);
    assert_eq!(world.archetypes.len(), 3);
    assert!(world.entity_locations.contains_key(&entity1));
    assert!(world.entity_locations.contains_key(&entity2));
    assert!(world.entity_locations.contains_key(&entity3));

    // Print archetype information
    world.print_archetypes();

    // Verify each archetype's component mask matches expected components
    for (archetype_id, archetype) in world.archetypes.iter() {
        println!("\n--- Verifying Archetype {:?} ---", archetype_id);

        // Get component names
        let comp_names: Vec<String> = archetype
            .component_types
            .iter()
            .filter_map(|component_id| {
                world
                    .component_registry
                    .get_name(component_id)
                    .map(String::from)
            })
            .collect();

        println!("Components: {:?}", comp_names);

        // Build expected mask from component types
        let mut expected_mask = ComponentMask::empty();
        for component_id in &archetype.component_types {
            if let Some(bit) = world.component_registry.get_bit(component_id) {
                expected_mask.set(bit);
                println!(
                    "  - {:?} -> bit {}",
                    world
                        .component_registry
                        .get_name(component_id)
                        .unwrap_or("Unknown"),
                    bit
                );
            }
        }

        // Verify masks match
        assert_eq!(
            archetype.component_mask, expected_mask,
            "Archetype {:?} mask mismatch!\nActual:   {:?}\nExpected: {:?}",
            archetype_id, archetype.component_mask, expected_mask
        );

        println!("✓ Mask verified: {:?}", archetype.component_mask);
    }

    println!("\n✓ All 3 archetypes verified successfully!");
}

/// Tests adding a new component to an existing entity.
///
/// This test verifies that:
/// - A component can be added to an entity that doesn't already have it
/// - The entity is migrated to a new archetype with the added component
/// - Existing components on the entity are preserved during migration
/// - The entity remains valid and tracked in the world
/// - The old archetype is automatically cleaned up when it becomes empty
///
/// Expected results:
/// - add_component should return true (success)
/// - The entity should still exist in entity_locations
/// - Old archetype should be automatically removed, leaving 1 archetype
#[test]
fn test_add_component_to_entity() {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();

    let entity = world
        .create_entity()
        .with(Position { x: 10.0, y: 20.0 })
        .with(Velocity { x: 1.0, y: 2.0 })
        .build()
        .unwrap();

    assert_eq!(
        world.archetypes.len(),
        1,
        "Should have 1 archetype initially"
    );

    // Add Health component
    let result = world.add_component(entity, Health { hp: 50 });

    assert!(result.is_ok(), "Should successfully add component");
    assert!(world.entity_locations.contains_key(&entity));

    // Since this is the only entity, the old archetype should be automatically removed
    assert_eq!(
        world.archetypes.len(),
        1,
        "Should have 1 archetype after adding Health (old one auto-removed)"
    );

    world.print_archetypes();
}

/// Tests attempting to add a component to a non-existent entity.
///
/// This test verifies that:
/// - The system handles invalid entity IDs gracefully
/// - No panic or crash occurs when operating on a fake entity
/// - The operation correctly returns failure status
///
/// Expected results:
/// - add_component should return false (failure)
/// - No side effects or modifications to the world state
#[test]
fn test_add_component_to_nonexistent_entity() {
    let mut world = World::new();
    world.register_component::<Position>();

    let fake_entity = crate::Entity::new_for_test(9999, 0);
    let result = world.add_component(fake_entity, Position { x: 0.0, y: 0.0 });

    assert_eq!(
        result,
        Err(AddComponentError::EntityNotFound),
        "Should fail to add component to non-existent entity"
    );
}

/// Tests removing a component from an entity that has multiple components.
///
/// This test verifies that:
/// - A specific component can be removed from an entity
/// - The entity is migrated to a new archetype without the removed component
/// - Other components remain intact on the entity
/// - The entity continues to exist in the world
///
/// Expected results:
/// - remove_component should return true (success)
/// - The entity should still be tracked in entity_locations
/// - The entity should be in a different archetype (Position+Health instead of Position+Velocity+Health)
#[test]
fn test_remove_component_from_entity() {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();

    let entity = world
        .create_entity()
        .with(Position { x: 100.0, y: 200.0 })
        .with(Velocity { x: 5.0, y: 10.0 })
        .with(Health { hp: 100 })
        .build()
        .unwrap();

    // Remove Velocity component
    let result = world.remove_component::<Velocity>(entity);

    assert_eq!(world.archetypes.len(), 1, "Should have 1 archetype");

    assert!(result.is_ok(), "Should successfully remove component");
    assert!(world.entity_locations.contains_key(&entity));

    let location = world.entity_locations.get(&entity).unwrap();
    let archetype = world.archetypes.get(&location.archetype_id).unwrap();

    // Archetype should now only have Position and Health
    assert_eq!(
        archetype.component_types.len(),
        2,
        "Should have 2 component types"
    );

    // Archetype should contain Position and Health, but not Velocity. Checking component IDs.
    // Verify component IDs are as expected
    let position_id = ComponentId::of::<Position>();
    let health_id = ComponentId::of::<Health>();
    let velocity_id = ComponentId::of::<Velocity>();

    assert!(
        archetype.component_types.contains(&position_id),
        "Archetype should contain Position component"
    );
    assert!(
        archetype.component_types.contains(&health_id),
        "Archetype should contain Health component"
    );
    assert!(
        !archetype.component_types.contains(&velocity_id),
        "Archetype should not contain Velocity component"
    );
}

/// Tests attempting to remove a component from a non-existent entity.
///
/// This test verifies that:
/// - The system handles invalid entity IDs gracefully during removal
/// - No panic occurs when trying to remove from a fake entity
/// - The operation correctly reports failure
///
/// Expected results:
/// - remove_component should return false (failure)
/// - No modifications to the world state
#[test]
fn test_remove_component_from_nonexistent_entity() {
    let mut world = World::new();
    world.register_component::<Velocity>();

    let fake_entity = crate::Entity::new_for_test(9999, 0);
    let result = world.remove_component::<Velocity>(fake_entity);

    assert_eq!(
        result,
        Err(RemoveComponentError::EntityNotFound),
        "Should fail to remove component from non-existent entity"
    );
}

/// Tests removing the last component from an entity, which should destroy it.
///
/// This test verifies that:
/// - When an entity's last component is removed, the entity is automatically destroyed
/// - No entities with zero components are left in the world
/// - The entity is properly removed from all tracking structures
/// - If entity count drops to zero, archetypes are cleaned up
///
/// Expected results:
/// - remove_component should return true (success)
/// - The entity count should drop to 0
/// - The entity should no longer exist in entity_locations
/// - All archetypes should be removed if no entities remain
#[test]
fn test_remove_last_component_destroys_entity() {
    let mut world = World::new();
    world.register_component::<Position>();

    let entity = world
        .create_entity()
        .with(Position { x: 5.0, y: 15.0 })
        .build()
        .unwrap();

    assert_eq!(world.entity_locations.len(), 1);

    // Remove the only component - should destroy entity
    let result = world.remove_component::<Position>(entity);

    assert!(result.is_ok(), "Should successfully remove component");
    assert_eq!(
        world.entity_locations.len(),
        0,
        "Entity should be destroyed"
    );
    assert!(!world.entity_locations.contains_key(&entity));

    assert!(world.archetypes.is_empty(), "No archetypes should remain");
}

/// Tests destroying an entity and verifying other entities remain unaffected.
///
/// This test verifies that:
/// - An entity can be completely removed from the world
/// - Destroying one entity doesn't affect other entities
/// - The entity is removed from its archetype and all tracking structures
/// - The total entity count decreases correctly
///
/// Expected results:
/// - destroy should return true (success)
/// - Entity count should decrease from 2 to 1
/// - The destroyed entity should no longer exist in entity_locations
/// - The other entity should remain unaffected
#[test]
fn test_destroy_entity() {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();

    let entity1 = world
        .create_entity()
        .with(Position { x: 10.0, y: 20.0 })
        .build()
        .unwrap();

    let entity2 = world
        .create_entity()
        .with(Position { x: 5.0, y: 15.0 })
        .with(Velocity { x: 1.0, y: 2.0 })
        .build()
        .unwrap();

    assert_eq!(world.entity_locations.len(), 2);

    // Destroy entity1
    let result = world.destroy_entity(entity1);

    assert!(result, "Should successfully destroy entity");
    assert_eq!(world.entity_locations.len(), 1);
    assert!(!world.entity_locations.contains_key(&entity1));
    assert!(world.entity_locations.contains_key(&entity2));
}

/// Tests attempting to destroy a non-existent entity.
///
/// This test verifies that:
/// - The system handles invalid entity IDs gracefully during destroy
/// - No panic or crash occurs when destroying a fake entity
/// - The operation correctly reports failure
///
/// Expected results:
/// - destroy should return false (failure)
/// - No changes to the world state
#[test]
fn test_destroy_nonexistent_entity() {
    let mut world = World::new();
    let fake_entity = crate::Entity::new_for_test(9999, 0);

    let result = world.destroy_entity(fake_entity);

    assert!(!result, "Should fail to destroy non-existent entity");
}

/// Tests that attempting to destroy an already-destroyed entity fails correctly.
///
/// This test verifies that:
/// - Once an entity is destroyed, it cannot be destroyed again
/// - The system properly tracks which entities exist vs don't exist
/// - Repeated destroy operations are safely rejected
///
/// Expected results:
/// - First destroy should return true (success)
/// - Second destroy should return false (entity no longer exists)
/// - No panic or invalid state from double-destroy attempt
#[test]
fn test_destroy_already_destroyed_entity() {
    let mut world = World::new();
    world.register_component::<Position>();

    let entity = world
        .create_entity()
        .with(Position { x: 10.0, y: 20.0 })
        .build()
        .unwrap();

    // First destroy should succeed
    let result1 = world.destroy_entity(entity);
    assert!(result1);

    // Second destroy should fail
    let result2 = world.destroy_entity(entity);
    assert!(!result2, "Should fail to destroy already-destroyed entity");
}

/// Tests the cleanup of empty archetypes after entities are destroyed.
///
/// This test verifies that:
/// - When all entities are removed from an archetype, it becomes empty
/// - The cleanup_empty_archetypes method removes unused archetypes
/// - Non-empty archetypes and their entities remain unaffected
/// - Memory is properly reclaimed from empty archetype storage
///
/// Expected results:
/// - Initially 2 archetypes should exist
/// - After destroying entity1 and cleanup, archetype count should decrease
/// - entity2 should still exist and be properly tracked
#[test]
fn test_cleanup_empty_archetypes() {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();

    // Create some entities
    let entity1 = world
        .create_entity()
        .with(Position { x: 10.0, y: 20.0 })
        .build()
        .unwrap();

    let entity2 = world
        .create_entity()
        .with(Position { x: 5.0, y: 15.0 })
        .with(Velocity { x: 1.0, y: 2.0 })
        .build()
        .unwrap();

    let initial_archetypes = world.archetypes.len();
    assert_eq!(initial_archetypes, 2);

    // Destroy one entity, leaving one archetype empty
    let _ = world.destroy_entity(entity1);

    // Cleanup should remove empty archetype
    world.cleanup_empty_archetypes();

    assert!(world.archetypes.len() < initial_archetypes);
    assert!(world.entity_locations.contains_key(&entity2));
}

/// Tests entity migration between archetypes when components are added and removed.
///
/// This test verifies that:
/// - Adding a component moves the entity to a different archetype
/// - Removing a component moves the entity to yet another archetype
/// - Each archetype change is properly tracked with different archetype IDs
/// - Component data is preserved during migrations
///
/// Expected results:
/// - Entity starts in archetype for (Position+Velocity)
/// - After adding Health, entity moves to archetype for (Position+Velocity+Health)
/// - After removing Velocity, entity moves to archetype for (Position+Health)
/// - All three archetype IDs should be different from each other
#[test]
fn test_entity_archetype_migration() {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();

    // Start with Position + Velocity
    let entity = world
        .create_entity()
        .with(Position { x: 10.0, y: 20.0 })
        .with(Velocity { x: 1.0, y: 2.0 })
        .build()
        .unwrap();

    let initial_location = *world.entity_locations.get(&entity).unwrap();

    // Add Health - should migrate to new archetype
    world.add_component(entity, Health { hp: 100 }).unwrap();

    let after_add_location = *world.entity_locations.get(&entity).unwrap();
    assert_ne!(
        initial_location.archetype_id, after_add_location.archetype_id,
        "Entity should be in different archetype after adding component"
    );

    // Remove Velocity - should migrate to another archetype
    world.remove_component::<Velocity>(entity).unwrap();

    let after_remove_location = *world.entity_locations.get(&entity).unwrap();
    assert_ne!(
        after_add_location.archetype_id, after_remove_location.archetype_id,
        "Entity should be in different archetype after removing component"
    );
}

/// Tests that empty archetypes are automatically cleaned up when last entity moves.
///
/// This test verifies that:
/// - When the last entity in an archetype is moved to another archetype, the empty one is removed
/// - The archetype is removed from both the archetypes map and the lookup table
/// - No manual cleanup_empty_archetypes() call is needed
/// - The world remains in a consistent state
///
/// Expected results:
/// - Initially 1 archetype exists (Position+Velocity)
/// - After adding Health, 2 archetypes exist temporarily
/// - The old archetype is automatically removed, leaving only 1 archetype
/// - The entity is correctly tracked in the new archetype
#[test]
fn test_automatic_empty_archetype_cleanup() {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();

    // Create single entity with Position + Velocity
    let entity = world
        .create_entity()
        .with(Position { x: 10.0, y: 20.0 })
        .with(Velocity { x: 1.0, y: 2.0 })
        .build()
        .unwrap();

    assert_eq!(
        world.archetypes.len(),
        1,
        "Should have 1 archetype initially"
    );

    // Add Health - this should move entity to new archetype
    // The old archetype should be automatically removed since it becomes empty
    world.add_component(entity, Health { hp: 100 }).unwrap();

    assert_eq!(
        world.archetypes.len(),
        1,
        "Should still have 1 archetype after migration (old one auto-removed)"
    );
    assert!(world.entity_locations.contains_key(&entity));

    // Verify the entity is in the correct archetype with all 3 components
    let location = world.entity_locations.get(&entity).unwrap();
    let archetype = world.archetypes.get(&location.archetype_id).unwrap();
    assert_eq!(
        archetype.component_types.len(),
        3,
        "Entity should have 3 components"
    );

    println!("✓ Empty archetype automatically cleaned up after entity migration");
}

/// Test that archetype print_info displays component names and entity count
#[test]
fn test_archetype_print_info() {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();

    // Create some entities
    world
        .create_entity()
        .with(Position { x: 10.0, y: 20.0 })
        .with(Velocity { x: 1.0, y: 2.0 })
        .build()
        .unwrap();

    world
        .create_entity()
        .with(Position { x: 5.0, y: 15.0 })
        .with(Velocity { x: 0.5, y: 1.5 })
        .build()
        .unwrap();

    // Print info using the world helper method
    world.print_archetypes();

    // Verify archetype structure
    assert_eq!(world.entity_locations.len(), 2, "Should have 2 entities");
    assert_eq!(world.archetypes.len(), 1, "Should have 1 archetype");

    // Get the archetype and verify its contents
    let archetype = world.archetypes.values().next().unwrap();
    assert_eq!(
        archetype.entities.len(),
        2,
        "Archetype should contain 2 entities"
    );
    assert_eq!(
        archetype.component_types.len(),
        2,
        "Archetype should have 2 component types"
    );

    // Verify component names are registered and retrievable
    let comp_names: Vec<String> = archetype
        .component_types
        .iter()
        .filter_map(|component_id| {
            world
                .component_registry
                .get_name(component_id)
                .map(String::from)
        })
        .collect();

    assert_eq!(comp_names.len(), 2, "Should have 2 component names");

    // Check that both expected component names are present
    let has_position = comp_names.iter().any(|name| name.contains("Position"));
    let has_velocity = comp_names.iter().any(|name| name.contains("Velocity"));

    assert!(
        has_position,
        "Should contain Position component, found: {:?}",
        comp_names
    );
    assert!(
        has_velocity,
        "Should contain Velocity component, found: {:?}",
        comp_names
    );

    println!("✓ Component names verified: {:?}", comp_names);
}

/// Tests entity generation system for safe ID recycling.
///
/// This test verifies that:
/// - Entity IDs are recycled after destruction
/// - Generations are incremented when IDs are reused
/// - Stale handles (old generation) cannot access recycled entities
/// - New entities with recycled IDs work correctly
///
/// Expected results:
/// - Destroyed entity's ID should be reused for new entity
/// - New entity should have same ID but different generation
/// - Old handle should be invalid (is_entity_valid returns false)
/// - Old handle should not access new entity's components
#[test]
fn test_entity_generations() {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();

    // Create first entity
    let entity1 = world
        .create_entity()
        .with(Position { x: 10.0, y: 20.0 })
        .build()
        .unwrap();

    println!("Entity1: id={}, gen={}", entity1.id, entity1.generation);
    assert_eq!(entity1.id, 0, "First entity should have ID 0");
    assert_eq!(
        entity1.generation, 0,
        "First entity should have generation 0"
    );

    // Verify entity1 exists and has component
    assert!(world.is_entity_valid(entity1), "Entity1 should be valid");
    assert!(
        world.get_component::<Position>(entity1).is_some(),
        "Entity1 should have Position"
    );

    // Destroy entity1
    let destroyed = world.destroy_entity(entity1);
    assert!(destroyed, "Entity1 should be destroyed successfully");

    // Verify entity1 is no longer valid
    assert!(
        !world.is_entity_valid(entity1),
        "Entity1 should be invalid after destruction"
    );
    assert!(
        world.get_component::<Position>(entity1).is_none(),
        "Destroyed entity should not have components"
    );

    // Create a new entity - should reuse ID 0 with generation 1
    let entity2 = world
        .create_entity()
        .with(Velocity { x: 5.0, y: 10.0 })
        .build()
        .unwrap();

    println!("Entity2: id={}, gen={}", entity2.id, entity2.generation);
    assert_eq!(entity2.id, 0, "New entity should reuse ID 0");
    assert_eq!(entity2.generation, 1, "New entity should have generation 1");

    // Verify entity2 is valid
    assert!(world.is_entity_valid(entity2), "Entity2 should be valid");
    assert!(
        world.get_component::<Velocity>(entity2).is_some(),
        "Entity2 should have Velocity"
    );

    // Critical: Old handle (entity1) should NOT access entity2's data
    assert!(
        !world.is_entity_valid(entity1),
        "Old handle should still be invalid"
    );
    assert!(
        world.get_component::<Velocity>(entity1).is_none(),
        "Old handle should not access new entity's components"
    );
    assert!(
        world.get_component::<Position>(entity1).is_none(),
        "Old handle should not access any components"
    );

    // Verify they are different entities (different hash/eq)
    assert_ne!(
        entity1, entity2,
        "Entities with different generations should not be equal"
    );

    println!("✓ Entity generation recycling works correctly!");
}

/// Tests multiple rounds of entity recycling.
///
/// This test verifies that:
/// - Multiple destroy/create cycles correctly increment generations
/// - The free list works correctly with multiple recycled IDs
/// - Generations wrap around safely (using wrapping_add)
#[test]
fn test_multiple_entity_recycling_rounds() {
    let mut world = World::new();
    world.register_component::<Position>();

    // Create and destroy the same ID multiple times
    let mut last_entity = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();
    assert_eq!(last_entity.id, 0);
    assert_eq!(last_entity.generation, 0);

    for round in 1..=5 {
        let old_entity = last_entity;
        let _ = world.destroy_entity(old_entity);

        let new_entity = world
            .create_entity()
            .with(Position {
                x: round as f32,
                y: 0.0,
            })
            .build()
            .unwrap();

        assert_eq!(new_entity.id, 0, "Should reuse ID 0 in round {}", round);
        assert_eq!(
            new_entity.generation, round,
            "Generation should be {} in round {}",
            round, round
        );

        // Old handle should be invalid
        assert!(!world.is_entity_valid(old_entity));
        // New handle should be valid
        assert!(world.is_entity_valid(new_entity));

        last_entity = new_entity;
    }

    println!("✓ Multiple recycling rounds work correctly!");
}

/// Tests that multiple entities can be recycled independently.
///
/// This test verifies LIFO (stack) behavior of the free list.
#[test]
fn test_free_list_lifo_order() {
    let mut world = World::new();
    world.register_component::<Position>();

    // Create 3 entities
    let entity0 = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();
    let entity1 = world
        .create_entity()
        .with(Position { x: 1.0, y: 1.0 })
        .build()
        .unwrap();
    let entity2 = world
        .create_entity()
        .with(Position { x: 2.0, y: 2.0 })
        .build()
        .unwrap();

    assert_eq!(entity0.id, 0);
    assert_eq!(entity1.id, 1);
    assert_eq!(entity2.id, 2);

    // Destroy in order: entity0, entity1, entity2
    let _ = world.destroy_entity(entity0);
    let _ = world.destroy_entity(entity1);
    let _ = world.destroy_entity(entity2);

    // Free list should be: [(0, 1), (1, 1), (2, 1)]
    // Pop order (LIFO): entity2's ID first, then entity1's, then entity0's

    let new_entity1 = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();
    assert_eq!(new_entity1.id, 2, "Should pop ID 2 first (LIFO)");
    assert_eq!(new_entity1.generation, 1);

    let new_entity2 = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();
    assert_eq!(new_entity2.id, 1, "Should pop ID 1 second");
    assert_eq!(new_entity2.generation, 1);

    let new_entity3 = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();
    assert_eq!(new_entity3.id, 0, "Should pop ID 0 third");
    assert_eq!(new_entity3.generation, 1);

    // Next entity should get a fresh ID
    let new_entity4 = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();
    assert_eq!(new_entity4.id, 3, "Should allocate fresh ID 3");
    assert_eq!(new_entity4.generation, 0);

    println!("✓ Free list LIFO order works correctly!");
}

/// A slot whose generation reaches `u32::MAX` is retired instead of
/// wrapping back to zero (audit 5.14 / 4.1).
///
/// Wrapping would resurrect every stale handle from 2^32 recycles ago -
/// the ABA failure where a handle silently addresses an unrelated entity.
/// The free list must drop the slot instead, so a stale handle can never
/// validate against a recycled entity.
#[test]
fn slot_at_generation_max_is_retired_not_wrapped() {
    let mut world = World::new();
    world.register_component::<Position>();

    // Seed the free list so the next create reuses id 7 at the
    // second-highest representable generation.
    world.free_entity_ids.push((7, u32::MAX - 1));

    // First life of the slot: a real entity near the ceiling.
    let stale = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();
    assert_eq!(stale.id, 7);
    assert_eq!(stale.generation, u32::MAX - 1);

    // Destroying it recycles the slot one step closer to the ceiling.
    assert!(world.destroy_entity(stale));
    assert_eq!(world.free_entity_ids.as_slice(), &[(7, u32::MAX)]);

    // Second life at the ceiling itself.
    let ceiling = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();
    assert_eq!(ceiling.generation, u32::MAX);

    // Destroying at the ceiling retires the slot instead of wrapping to 0.
    assert!(world.destroy_entity(ceiling));
    assert!(
        world.free_entity_ids.is_empty(),
        "the slot must be retired, not wrapped back to generation 0"
    );

    // The id is never handed out again, so the stale handle can never
    // validate against a recycled entity.
    let replacement = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();
    assert_ne!(replacement.id, 7, "a retired slot must not be recycled");
    assert!(!world.is_entity_valid(stale), "the stale handle stays dead");
}

/// Tests entity generations with multiple archetypes and component removal.
///
/// This test verifies that:
/// - Entities in different archetypes have independent generation tracking
/// - Removing components (which moves entity to new archetype) preserves entity identity
/// - Destroying entities from different archetypes correctly adds IDs to free list
/// - Recycled IDs work correctly regardless of which archetype the original was in
#[test]
fn test_generations_with_multiple_archetypes_and_component_removal() {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();

    // Create 3 entities:
    // entity1, entity2: Position + Velocity (same archetype)
    // entity3: Position + Health (different archetype)
    let entity1 = world
        .create_entity()
        .with(Position { x: 1.0, y: 1.0 })
        .with(Velocity { x: 10.0, y: 10.0 })
        .build()
        .unwrap();

    let entity2 = world
        .create_entity()
        .with(Position { x: 2.0, y: 2.0 })
        .with(Velocity { x: 20.0, y: 20.0 })
        .build()
        .unwrap();

    let entity3 = world
        .create_entity()
        .with(Position { x: 3.0, y: 3.0 })
        .with(Health { hp: 100 })
        .build()
        .unwrap();

    println!(
        "Created: entity1(id={}, gen={}), entity2(id={}, gen={}), entity3(id={}, gen={})",
        entity1.id,
        entity1.generation,
        entity2.id,
        entity2.generation,
        entity3.id,
        entity3.generation
    );

    assert_eq!(entity1.id, 0);
    assert_eq!(entity2.id, 1);
    assert_eq!(entity3.id, 2);
    assert_eq!(world.archetypes.len(), 2, "Should have 2 archetypes");

    // Remove Velocity from entity1 - moves it to Position-only archetype
    let old_entity1 = entity1;
    let removed = world.remove_component::<Velocity>(entity1);
    assert!(removed.is_ok(), "Should remove Velocity from entity1");

    // entity1 should still be valid with same id and generation (entity wasn't destroyed)
    assert!(
        world.is_entity_valid(entity1),
        "entity1 should still be valid after component removal"
    );
    assert!(
        world.get_component::<Position>(entity1).is_some(),
        "entity1 should still have Position"
    );
    assert!(
        world.get_component::<Velocity>(entity1).is_none(),
        "entity1 should not have Velocity"
    );

    // Destroy entity2 (from Position+Velocity archetype)
    let old_entity2 = entity2;
    let _ = world.destroy_entity(entity2);
    assert!(
        !world.is_entity_valid(old_entity2),
        "entity2 should be invalid after destruction"
    );

    // Destroy entity3 (from Position+Health archetype)
    let old_entity3 = entity3;
    let _ = world.destroy_entity(entity3);
    assert!(
        !world.is_entity_valid(old_entity3),
        "entity3 should be invalid after destruction"
    );

    // Free list should now have: [(1, 1), (2, 1)] (LIFO order)
    // entity1 (id=0) is still alive

    // Create new entity - should reuse ID 2 (last destroyed)
    let new_entity1 = world
        .create_entity()
        .with(Health { hp: 50 })
        .build()
        .unwrap();

    println!(
        "new_entity1: id={}, gen={}",
        new_entity1.id, new_entity1.generation
    );
    assert_eq!(new_entity1.id, 2, "Should reuse ID 2 (LIFO)");
    assert_eq!(new_entity1.generation, 1, "Should have generation 1");

    // Old entity3 handle should NOT access new_entity1's data
    assert!(
        !world.is_entity_valid(old_entity3),
        "Old entity3 handle should be invalid"
    );
    assert!(
        world.get_component::<Health>(old_entity3).is_none(),
        "Old handle should not access new entity"
    );

    // Create another entity - should reuse ID 1
    let new_entity2 = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .with(Velocity { x: 0.0, y: 0.0 })
        .build()
        .unwrap();

    println!(
        "new_entity2: id={}, gen={}",
        new_entity2.id, new_entity2.generation
    );
    assert_eq!(new_entity2.id, 1, "Should reuse ID 2");
    assert_eq!(new_entity2.generation, 1, "Should have generation 1");

    // Old entity2 handle should NOT access new_entity2's data
    assert!(
        !world.is_entity_valid(old_entity2),
        "Old entity2 handle should be invalid"
    );

    // Verify entity1 (never destroyed) still works with original handle
    assert!(
        world.is_entity_valid(old_entity1),
        "Original entity1 should still be valid"
    );
    let pos = world.get_component::<Position>(old_entity1).unwrap();
    assert_eq!(pos.x, 1.0, "entity1 Position should be preserved");

    // Destroy entity1 and verify recycling
    let _ = world.destroy_entity(entity1);
    assert!(
        !world.is_entity_valid(old_entity1),
        "entity1 should be invalid after destruction"
    );

    let new_entity3 = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();
    println!(
        "new_entity3: id={}, gen={}",
        new_entity3.id, new_entity3.generation
    );
    assert_eq!(new_entity3.id, 0, "Should reuse ID 1");
    assert_eq!(new_entity3.generation, 1, "Should have generation 1");

    println!("✓ Generations with multiple archetypes and component removal work correctly!");
}

/// Tests that component data is correctly swap-removed when an entity is destroyed.
///
/// This test exposes the bug where component data is NOT swap-removed from storage
/// when an entity is destroyed, causing remaining entities to read stale/wrong data.
///
/// Expected behavior
/// - After destroying entity0, entity2 should still have its original Position (2.0, 2.0)
/// - Currently, entity2 reads entity0's old Position (0.0, 0.0) - BUG!
#[test]
fn test_component_swap_remove_on_destroy() {
    let mut world = World::new();
    world.register_component::<Position>();

    // Create 3 entities in the same archetype
    let entity0 = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();
    let entity1 = world
        .create_entity()
        .with(Position { x: 1.0, y: 1.0 })
        .build()
        .unwrap();
    let entity2 = world
        .create_entity()
        .with(Position { x: 2.0, y: 2.0 })
        .build()
        .unwrap();

    // Verify initial state
    assert_eq!(world.get_component::<Position>(entity0).unwrap().x, 0.0);
    assert_eq!(world.get_component::<Position>(entity1).unwrap().x, 1.0);
    assert_eq!(world.get_component::<Position>(entity2).unwrap().x, 2.0);

    // Archetype entity list: [entity0, entity1, entity2] (indices 0, 1, 2)
    // Component storage:     [Pos(0,0), Pos(1,1), Pos(2,2)]

    // Destroy entity0 (index 0)
    // Entity list swap_remove: entity2 moves from index 2 to index 0
    // Entity list becomes: [entity2, entity1] (entity2 now at index 0)
    //
    // BUG: Component storage is NOT updated!
    // Component storage still: [Pos(0,0), Pos(1,1), Pos(2,2)]
    //
    // Now entity2 has index 0, but component at index 0 is Pos(0,0) - WRONG!
    let _ = world.destroy_entity(entity0);

    // entity1 should still have its original position (index 1 unchanged)
    let pos1 = world.get_component::<Position>(entity1).unwrap();
    assert_eq!(pos1.x, 1.0, "entity1 Position.x should be 1.0");
    assert_eq!(pos1.y, 1.0, "entity1 Position.y should be 1.0");

    // entity2 was swapped to index 0 - it should still have Position(2.0, 2.0)
    // BUG: It actually reads Position(0.0, 0.0) because component storage wasn't swap-removed
    let pos2 = world.get_component::<Position>(entity2).unwrap();

    println!(
        "entity2 Position after entity0 destroyed: ({}, {})",
        pos2.x, pos2.y
    );
    println!("Expected: (2.0, 2.0), Got: ({}, {})", pos2.x, pos2.y);

    // This assertion FAILS because of the unimplemented component swap_remove
    assert_eq!(
        pos2.x, 2.0,
        "BUG: entity2 should have Position.x = 2.0, but got {} (entity0's old data)",
        pos2.x
    );
    assert_eq!(
        pos2.y, 2.0,
        "BUG: entity2 should have Position.y = 2.0, but got {} (entity0's old data)",
        pos2.y
    );

    println!("✓ Component swap_remove works correctly!");
}

/// Tests that component data is properly cleaned up when an entity migrates between archetypes.
///
/// When an entity gains or loses a component, it moves to a different archetype.
/// The old archetype must properly remove the entity's component data using swap_remove.
/// Otherwise:
/// 1. Memory leaks occur (orphaned component data)
/// 2. Other entities in the old archetype may read wrong component data
///
/// This test verifies:
/// - Component data is removed from old archetype during migration
/// - Other entities in the old archetype still have correct component data
/// - The swapped entity (if any) correctly maps to its swapped component data
#[test]
fn test_component_cleanup_on_archetype_migration() {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    world.register_component::<Health>();

    // Create 3 entities with Position + Velocity in the same archetype
    let entity0 = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .with(Velocity { x: 100.0, y: 100.0 })
        .build()
        .unwrap();
    let entity1 = world
        .create_entity()
        .with(Position { x: 1.0, y: 1.0 })
        .with(Velocity { x: 101.0, y: 101.0 })
        .build()
        .unwrap();
    let entity2 = world
        .create_entity()
        .with(Position { x: 2.0, y: 2.0 })
        .with(Velocity { x: 102.0, y: 102.0 })
        .build()
        .unwrap();

    // Verify initial state
    assert_eq!(
        world.archetypes.len(),
        1,
        "Should have 1 archetype initially"
    );

    // Verify all entities have correct data
    assert_eq!(world.get_component::<Position>(entity0).unwrap().x, 0.0);
    assert_eq!(world.get_component::<Velocity>(entity0).unwrap().x, 100.0);
    assert_eq!(world.get_component::<Position>(entity1).unwrap().x, 1.0);
    assert_eq!(world.get_component::<Velocity>(entity1).unwrap().x, 101.0);
    assert_eq!(world.get_component::<Position>(entity2).unwrap().x, 2.0);
    assert_eq!(world.get_component::<Velocity>(entity2).unwrap().x, 102.0);

    // Now add Health to entity0 - this moves it to a NEW archetype (Position+Velocity+Health)
    // The old archetype (Position+Velocity) should swap_remove entity0's data
    // entity2 should be swapped into index 0
    world.add_component(entity0, Health { hp: 50 }).unwrap();

    // Verify entity0 moved to new archetype and has all components
    assert!(world.get_component::<Position>(entity0).is_some());
    assert!(world.get_component::<Velocity>(entity0).is_some());
    assert!(world.get_component::<Health>(entity0).is_some());
    assert_eq!(world.get_component::<Position>(entity0).unwrap().x, 0.0);
    assert_eq!(world.get_component::<Velocity>(entity0).unwrap().x, 100.0);
    assert_eq!(world.get_component::<Health>(entity0).unwrap().hp, 50);

    // CRITICAL: entity1 and entity2 should still have correct data in old archetype
    // If swap_remove wasn't applied to component storage, entity2 (now at index 0)
    // would incorrectly read entity0's old data!

    let pos1 = world.get_component::<Position>(entity1).unwrap();
    let vel1 = world.get_component::<Velocity>(entity1).unwrap();
    assert_eq!(pos1.x, 1.0, "entity1 Position.x should be 1.0");
    assert_eq!(vel1.x, 101.0, "entity1 Velocity.x should be 101.0");

    let pos2 = world.get_component::<Position>(entity2).unwrap();
    let vel2 = world.get_component::<Velocity>(entity2).unwrap();
    assert_eq!(
        pos2.x, 2.0,
        "entity2 Position.x should be 2.0, but got {} (possible swap_remove bug)",
        pos2.x
    );
    assert_eq!(
        vel2.x, 102.0,
        "entity2 Velocity.x should be 102.0, but got {} (possible swap_remove bug)",
        vel2.x
    );

    // Verify archetype count (old one should still exist with entity1, entity2)
    assert_eq!(world.archetypes.len(), 2, "Should have 2 archetypes now");

    // Now remove Velocity from entity1 - moves to Position-only archetype
    world.remove_component::<Velocity>(entity1).unwrap();

    // entity2 should still have correct data (it's now alone in Position+Velocity archetype)
    let pos2 = world.get_component::<Position>(entity2).unwrap();
    let vel2 = world.get_component::<Velocity>(entity2).unwrap();
    assert_eq!(pos2.x, 2.0, "entity2 Position.x should still be 2.0");
    assert_eq!(vel2.x, 102.0, "entity2 Velocity.x should still be 102.0");

    // entity1 should only have Position now
    assert!(world.get_component::<Position>(entity1).is_some());
    assert!(world.get_component::<Velocity>(entity1).is_none());
    assert_eq!(world.get_component::<Position>(entity1).unwrap().x, 1.0);

    println!("✓ Component cleanup on archetype migration works correctly!");
}

/// Tests that `IteratorTimings` detects duplicate labels within a frame.
///
/// Two iterators with the same label will corrupt the per-label splitting hint.
/// This test simulates the logic inside `ParQueryIter::for_each`.
#[test]
fn test_per_label_duplicate_detection() {
    let timing = std::sync::Mutex::new(IteratorTimings::new());

    // Simulate iterator-1 with label "physics".
    {
        let mut t = timing.lock().unwrap();
        assert!(!t.visited_iterator_labels.contains(&"physics"));
        t.visited_iterator_labels.push("physics");
        t.per_iterator_label_average_duration
            .insert("physics", 120_000);
    }

    // Simulate iterator-2 with label "ai" - different label, no duplicate.
    {
        let mut t = timing.lock().unwrap();
        assert!(!t.visited_iterator_labels.contains(&"ai"));
        t.visited_iterator_labels.push("ai");
        t.per_iterator_label_average_duration.insert("ai", 50_000);
    }

    // Simulate a second "physics" iterator - same label, DUPLICATE.
    {
        let mut t = timing.lock().unwrap();
        assert!(t.visited_iterator_labels.contains(&"physics"));
        t.visited_duplicated_iterator_labels.push("physics");
        // Overwrites the splitting hint - exactly the problem we're detecting.
        t.per_iterator_label_average_duration
            .insert("physics", 800_000);
    }

    let t = timing.lock().unwrap();
    assert_eq!(t.visited_duplicated_iterator_labels, vec!["physics"]);
    assert_eq!(t.visited_iterator_labels, vec!["physics", "ai"]);
    // "physics" splitting hint was corrupted by the second write.
    assert_eq!(t.per_iterator_label_average_duration["physics"], 800_000);

    println!("✓ Duplicate label detection works correctly!");
}

/// The editor's Hierarchy source: `entity_rows` lists every live entity
/// with its component names, sorted by entity id.
#[test]
fn entity_rows_list_live_entities_with_components() {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();

    let a = world
        .create_entity()
        .with(Position { x: 1.0, y: 2.0 })
        .with(Velocity { x: 0.0, y: 0.0 })
        .build()
        .unwrap();
    let _b = world
        .create_entity()
        .with(Position { x: 3.0, y: 4.0 })
        .build()
        .unwrap();

    let rows = world.entity_rows();
    assert_eq!(rows.len(), 2);
    // Deterministic ordering by entity id: `a` was created first (id 0).
    assert_eq!(rows[0].entity, a);
    assert_eq!(rows[0].components.len(), 2);
    assert!(rows[0]
        .components
        .iter()
        .any(|name| name.contains("Position")));
    assert!(rows[0]
        .components
        .iter()
        .any(|name| name.contains("Velocity")));
    assert_eq!(rows[1].components.len(), 1);

    // A destroyed entity no longer appears and reports no names.
    let _ = world.destroy_entity(a);
    assert_eq!(world.entity_rows().len(), 1);
    assert_eq!(world.entity_component_names(a), None);
}

/// `entity_component_names` includes runtime-defined (descriptor)
/// components, and `resolve_entity_component_id` is scoped to the entity's
/// archetype.
#[test]
fn descriptor_components_appear_and_resolution_is_archetype_scoped() {
    let mut world = World::new();
    world.register_component::<Position>();
    let descriptor = world
        .register_component_descriptor(
            0xABCD,
            "Demo.Thing",
            4,
            4,
            99,
            Blittability::engine_verified(),
        )
        .unwrap();

    let with_descriptor = world
        .create_descriptor_entity(&[(descriptor, 7_u32.to_ne_bytes().to_vec())])
        .unwrap();
    let with_position = world
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();

    let names = world
        .entity_component_names(with_descriptor)
        .expect("entity alive");
    assert_eq!(names, vec!["Demo.Thing"]);

    // Resolution is per-entity: the descriptor component only resolves on
    // the entity that carries it, and Position only on its own entity.
    assert_eq!(
        world.resolve_entity_component_id(with_descriptor, "Demo.Thing"),
        Some(descriptor)
    );
    assert_eq!(
        world.resolve_entity_component_id(with_position, "Demo.Thing"),
        None
    );
    assert_eq!(
        world.resolve_entity_component_id(with_position, &type_name_of::<Position>()),
        Some(ComponentId::of::<Position>())
    );
}

/// `registered_components` lists every type (native and descriptor), sorted.
#[test]
fn registered_components_lists_every_type_sorted() {
    let mut world = World::new();
    world.register_component::<Velocity>();
    world.register_component::<Position>();
    world
        .register_component_descriptor(
            0x1111,
            "Demo.Alpha",
            4,
            4,
            1,
            Blittability::engine_verified(),
        )
        .unwrap();

    let registered = world.registered_components();
    assert_eq!(registered.len(), 3);
    // Sorted by name (full type paths sort before the demo name here).
    let names: Vec<String> = registered.iter().map(|(name, _)| name.clone()).collect();
    assert_eq!(names[0], "Demo.Alpha");
    assert!(names[1].contains("Position"));
    assert!(names[2].contains("Velocity"));
}

/// A tiny helper to get a component's registered type name without
/// depending on the registry ordering in this test module.
fn type_name_of<T: 'static>() -> String {
    std::any::type_name::<T>().to_string()
}

// -------------------------------------------------------------------------
// Resource re-homing
// -------------------------------------------------------------------------

/// Counts its own drops, so a re-home that corrupted the stored function
/// table shows up as a missing or doubled drop rather than as silence.
///
/// The counter is owned per instance rather than being a `static`: the
/// harness runs these tests in parallel, and a shared counter makes every
/// `before + 1` assertion race with the others.
#[derive(Debug)]
struct RehomeProbe(std::sync::Arc<std::sync::atomic::AtomicUsize>);
impl crate::resource::Resource for RehomeProbe {}

impl Drop for RehomeProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A fresh probe and the counter watching it.
fn rehome_probe() -> (RehomeProbe, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let drops = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    (RehomeProbe(std::sync::Arc::clone(&drops)), drops)
}

/// Reads one probe's counter.
fn drops_of(counter: &std::sync::atomic::AtomicUsize) -> usize {
    counter.load(std::sync::atomic::Ordering::SeqCst)
}

/// Inserting a resource records its function table, so a later reload has
/// something to re-home from even if it never inserts the value again.
#[test]
fn inserting_a_resource_records_its_function_table() {
    let mut world = World::new();
    let id = crate::resource::ResourceId::of::<RehomeProbe>();

    assert!(!world.resource_factories.contains_key(&id));
    world.insert_resource(rehome_probe().0);
    assert!(world.resource_factories.contains_key(&id));
}

/// `register_resource` records the table without inserting a value, which
/// is how a reloaded generation keeps an existing resource re-homeable.
#[test]
fn registering_a_resource_records_the_table_without_a_value() {
    let mut world = World::new();
    let id = crate::resource::ResourceId::of::<RehomeProbe>();

    world.register_resource::<RehomeProbe>();

    assert!(world.resource_factories.contains_key(&id));
    assert!(
        !world.has_resource::<RehomeProbe>(),
        "no value was inserted"
    );
}

/// Re-homing leaves the value intact and still droppable exactly once.
#[test]
fn rehoming_resources_preserves_the_value_and_its_drop() {
    let (probe, drops) = rehome_probe();
    let mut world = World::new();
    world.insert_resource(probe);

    // Stands in for a reloaded generation re-registering the type.
    world.register_resource::<RehomeProbe>();
    world.rehome_resources();

    assert!(world.has_resource::<RehomeProbe>(), "the value survives");
    assert_eq!(drops_of(&drops), 0, "nothing was dropped");

    drop(world);
    assert_eq!(
        drops_of(&drops),
        1,
        "the value is dropped exactly once after a re-home"
    );
}

/// The registration log reports what one `init` claimed, which is what
/// distinguishes a retired owner from a live one.
///
/// `resource_factories` cannot answer that: it accumulates and is never
/// pruned, so a retired module's entry looks identical to a live one.
#[test]
fn the_registration_log_reports_what_one_generation_claimed() {
    let mut world = World::new();
    world.insert_resource(rehome_probe().0);

    // Stands in for the moment just before a module's `init` runs.
    let before_init = world.resource_registration_sequence();
    world.register_resource::<artifact_a::Settings>();

    assert_eq!(
        world.resource_ids_registered_since(before_init),
        vec![crate::resource::ResourceId::of::<artifact_a::Settings>()],
        "only what this generation registered, not everything ever registered"
    );
}

/// Dropping a retired owner's resource releases its value, which is what
/// the host must do while the owning image is still mapped.
#[test]
fn dropping_a_retired_owners_resource_releases_its_value() {
    let (probe, drops) = rehome_probe();
    let mut world = World::new();
    world.insert_resource(probe);
    let id = crate::resource::ResourceId::of::<RehomeProbe>();

    assert_eq!(world.drop_resources(&[id]), 1);
    assert_eq!(drops_of(&drops), 1, "the value was dropped, not leaked");
    assert!(!world.has_resource::<RehomeProbe>());
    // Its bookkeeping goes too, so a later insert starts clean.
    assert!(!world.resource_factories.contains_key(&id));
}

/// Dropping an id the world does not hold is harmless and reports zero.
#[test]
fn dropping_an_absent_resource_is_a_no_op() {
    let mut world = World::new();
    let id = crate::resource::ResourceId::of::<RehomeProbe>();
    assert_eq!(world.drop_resources(&[id]), 0);
}

/// Removing a resource forgets its table, so a later resource registered
/// under the same id cannot inherit a stale one.
#[test]
fn removing_a_resource_forgets_its_function_table() {
    let mut world = World::new();
    let id = crate::resource::ResourceId::of::<RehomeProbe>();
    world.insert_resource(rehome_probe().0);

    world
        .remove_resource::<RehomeProbe>()
        .expect("it was there");

    assert!(!world.resource_factories.contains_key(&id));
}

// -------------------------------------------------------------------------
// Shared resource identity
// -------------------------------------------------------------------------

// A shared resource type, standing for one artifact's copy. The
// cross-artifact behaviour itself lives in
// `tests/shared_resource_identity.rs`; what stays here needs access to the
// world's private bookkeeping, which an integration test cannot reach.
mod artifact_a {
    /// `demo::Settings` as one artifact compiled it.
    #[derive(Debug)]
    pub struct Settings {
        // The shape is what the identity checks carry; nothing reads the
        // value.
        #[allow(dead_code)]
        pub value: u32,
    }
    impl crate::resource::Resource for Settings {
        fn shared_name() -> Option<&'static str> {
            Some("demo::Settings")
        }
    }
}

/// An ordinary resource is untouched: its id is its `TypeId`, and the box
/// still checks that exactly.
#[test]
fn an_ordinary_resource_keeps_the_strict_identity_check() {
    let mut world = World::new();
    world.insert_resource(rehome_probe().0);

    assert!(!world
        .resources
        .get(&crate::resource::ResourceId::of::<RehomeProbe>())
        .unwrap()
        .has_shared_identity());
    assert!(world.take_registration_error().is_none());
}

/// A released bit cannot alias two component sets: the archetype that
/// carried it is gone before the bit is reusable, so the next type to take
/// it gets its own column and its own archetype.
#[test]
fn recycled_bit_does_not_alias_archetypes() {
    let mut world = World::new();
    world.register_component::<Position>();
    world.register_component::<Velocity>();
    let velocity_id = ComponentId::of::<Velocity>();
    let released_bit = world
        .component_registry()
        .get_bit(&velocity_id)
        .expect("registered");
    let kept = world
        .create_entity()
        .with(Position { x: 1.0, y: 2.0 })
        .with(Velocity { x: 0.0, y: 0.0 })
        .build()
        .unwrap();

    let dropped = world.drop_forgotten_components(&[std::any::type_name::<Velocity>().to_string()]);
    assert_eq!(dropped, 1, "the entity's velocity row was rehomed out");
    assert_eq!(
        world.component_registry().get_bit(&velocity_id),
        None,
        "the registration is gone with the rows"
    );

    // The next registration takes the freed bit...
    world.register_component::<Health>();
    let health_id = ComponentId::of::<Health>();
    assert_eq!(
        world.component_registry().get_bit(&health_id),
        Some(released_bit),
        "the freed bit is the one reused"
    );

    // ...and the archetype it creates is its own, holding exactly its row.
    let health_entity = world
        .create_entity()
        .with(Position { x: 3.0, y: 4.0 })
        .with(Health { hp: 5 })
        .build()
        .unwrap();
    assert_eq!(world.live_row_count(health_id), 1);
    assert_eq!(world.live_row_count(ComponentId::of::<Position>()), 2);
    assert_eq!(
        world.archetypes.len(),
        2,
        "position-only and position+health are two archetypes"
    );
    let location = world.entity_locations[&health_entity];
    let archetype = &world.archetypes[&location.archetype_id];
    assert!(archetype.component_types.contains(&health_id));
    assert!(!archetype.component_types.contains(&velocity_id));
    assert!(world.entity_locations.contains_key(&kept));
}

/// A re-registration updates the id's stamp instead of adding an entry:
/// the window stays proportional to distinct resources, and a generation
/// that re-registers its resources is exactly what the host's reload diff
/// has to see.
#[test]
fn a_re_registered_resource_stays_one_registration() {
    let mut world = World::new();
    world.insert_resource(AccountedResource { value: 1 });
    let sequence = world.resource_registration_sequence();

    world.insert_resource(AccountedResource { value: 2 });
    world.insert_resource(AccountedResource { value: 3 });
    assert_eq!(
        world.get_resource::<AccountedResource>().unwrap().value,
        3,
        "the replacement really replaced the value"
    );
    let id = crate::resource::ResourceId::of::<AccountedResource>();
    assert_eq!(
        world.resource_ids_registered_since(sequence),
        vec![id],
        "a re-registration inside the window is reported, exactly once"
    );

    world.insert_resource(FreshAccountedResource { value: 4 });
    assert_eq!(
        world
            .get_resource::<FreshAccountedResource>()
            .unwrap()
            .value,
        4
    );
    let mut claimed = world.resource_ids_registered_since(sequence);
    claimed.sort_unstable();
    let mut expected = vec![
        id,
        crate::resource::ResourceId::of::<FreshAccountedResource>(),
    ];
    expected.sort_unstable();
    assert_eq!(
        claimed, expected,
        "the fresh id joins the window; nothing accumulates per insert"
    );
    assert_eq!(
        world.resource_registration_stamps.len(),
        2,
        "one entry per distinct id, however many times it was written"
    );
}

/// A resource another subject still claims is not dropped by one subject's
/// retirement: the claim refcount keeps the value alive until the last
/// claimant lets go.
#[test]
fn a_shared_claim_survives_one_subjects_retirement() {
    let mut world = World::new();
    let id = crate::resource::ResourceId::of::<AccountedResource>();
    world.insert_resource(AccountedResource { value: 1 });

    // Two subjects registered it.
    world.retain_resource_claims(&[id]);
    world.retain_resource_claims(&[id]);

    // One retires it: the value stays, claim and all.
    world.release_resource_claims(&[id]);
    assert_eq!(
        world.drop_resources(&[id]),
        0,
        "a live claim keeps the value standing"
    );
    assert!(world.get_resource::<AccountedResource>().is_some());

    // The last claimant retires it too.
    world.release_resource_claims(&[id]);
    assert_eq!(world.drop_resources(&[id]), 1, "the last release frees it");
    assert!(world.get_resource::<AccountedResource>().is_none());
}

/// A relayout republishes the whole registry layout - alignment and schema
/// hash included - so `get_layout` never describes a column that is gone.
#[test]
fn relayout_republishes_the_registry_layout() {
    let mut world = World::new();
    let component_id = world
        .register_component_descriptor(
            0xD9,
            "Project.Republished",
            8,
            4,
            100,
            Blittability::engine_verified(),
        )
        .unwrap();
    world
        .create_descriptor_entity(&[(component_id, vec![0; 8])])
        .unwrap();

    world
        .relayout_descriptor_component(component_id, 16, 8, 200, &FieldPlan::new())
        .expect("the empty plan fits both layouts");

    let record = world
        .component_registry()
        .get_layout(&component_id)
        .expect("registered");
    assert_eq!(record.size, 16);
    assert_eq!(
        record.align, 8,
        "the registration-time placeholder alignment moved with the storage"
    );
    assert_eq!(record.schema_hash, Some(200));
    let Some(StorageFactory::Descriptor(layout)) = world.storage_factories.get(&component_id)
    else {
        panic!("the factory is not a descriptor factory");
    };
    assert_eq!(
        (layout.size, layout.align, layout.schema_hash),
        (record.size, record.align, record.schema_hash.unwrap())
    );
}

// -------------------------------------------------------------------------
// Foreign resources
// -------------------------------------------------------------------------

/// A declared name hashes the engine's way, so a declaration from another
/// language and a Rust type that writes down the same string are one
/// resource - which is the whole point of the name being declared rather
/// than derived.
#[test]
fn a_foreign_declaration_hashes_to_the_rust_types_id() {
    let mut world = World::new();
    let id = world
        .register_foreign_resource("demo::Settings", "Project.Settings", 4, 4, 7)
        .expect("a fresh name is claimed");

    assert_eq!(
        id,
        crate::resource::ResourceId::of::<artifact_a::Settings>()
    );
    assert_eq!(
        id,
        crate::resource::ResourceId::Shared(crate::component::shared_component_identity(
            "demo::Settings"
        ))
    );
    assert_eq!(world.foreign_resource_layout(id), Some((4, 4, 7)));
    assert_eq!(
        world.shared_resource_names(),
        vec!["demo::Settings".to_string()]
    );
}

/// The claim guard keeps its Rust-versus-Rust name check and takes the
/// layout as the cross-language one: a matching declaration joins, a
/// mismatched one is refused.
#[test]
fn a_foreign_declaration_joins_by_layout_and_is_refused_by_a_mismatch() {
    let mut world = World::new();
    world.register_resource::<artifact_a::Settings>();
    assert!(world.take_registration_error().is_none());

    let id = world
        .register_foreign_resource("demo::Settings", "Project.Settings", 4, 4, 7)
        .expect("the same layout joins");
    assert_eq!(
        id,
        crate::resource::ResourceId::of::<artifact_a::Settings>()
    );
    assert!(
        world.take_registration_error().is_none(),
        "a matching declaration is not a conflict"
    );

    let error = world
        .register_foreign_resource("demo::Settings", "Project.Settings", 16, 8, 7)
        .expect_err("a different layout cannot be joined");
    assert!(matches!(
        error,
        WorldError::SharedResourceLayoutMismatch { .. }
    ));
    assert!(
        world.take_registration_error().is_none(),
        "the refusal goes back to the caller that made it, not to the next init drain"
    );
}

/// Bytes round-trip through a declaration, the size is checked, and a
/// mutable view stamps the change tick as it is handed out.
#[test]
fn foreign_bytes_round_trip_and_stamp_the_change_tick() {
    let mut world = World::new();
    let id = world
        .register_foreign_resource("demo::Bytes", "Project.Bytes", 8, 4, 7)
        .expect("a fresh name is claimed");
    assert!(world.foreign_resource_bytes(id).is_none(), "no value yet");

    world
        .insert_foreign_resource_bytes(id, &[1, 2, 3, 4, 5, 6, 7, 8])
        .expect("the payload matches the size");
    assert_eq!(
        world.foreign_resource_bytes(id),
        Some([1_u8, 2, 3, 4, 5, 6, 7, 8].as_slice())
    );

    assert!(matches!(
        world.insert_foreign_resource_bytes(id, &[0; 4]),
        Err(WorldError::ForeignResourceBytesMismatch { .. })
    ));
    assert_eq!(
        world.foreign_resource_bytes(id).unwrap().len(),
        8,
        "a refused payload leaves the stored value alone"
    );

    world.increment_change_tick();
    let expected_tick = world.change_tick();
    let (bytes, ticks) = world
        .foreign_resource_bytes_mut(id)
        .expect("the value is there");
    bytes[0] = 9;
    assert_eq!(ticks.changed, expected_tick);
    assert_eq!(world.foreign_resource_bytes(id).unwrap()[0], 9);
}

/// A private resource has no byte view: its id is a `TypeId`, which no
/// other language can name, so there is nothing for one to address.
#[test]
fn a_private_resource_has_no_foreign_view() {
    let mut world = World::new();
    world.insert_resource(rehome_probe().0);
    let id = crate::resource::ResourceId::of::<RehomeProbe>();

    assert!(world.foreign_resource_bytes(id).is_none());
    assert!(world.foreign_resource_bytes_mut(id).is_none());
    assert!(matches!(
        world.insert_foreign_resource_bytes(id, &[0; 8]),
        Err(WorldError::SharedResourceNotRegistered { .. })
    ));
    assert!(matches!(
        world.relayout_foreign_resource(id, 8, 4, 1, &FieldPlan::new()),
        Err(WorldError::ForeignResourceNotRegistered { .. })
    ));
}

/// A relayout moves the stored bytes through the plan, updates the declared
/// layout, and reports how many values it migrated.
#[test]
fn relayout_migrates_a_foreign_value_through_the_plan() {
    let mut world = World::new();
    let id = world
        .register_foreign_resource("demo::Relayout", "Project.Relayout", 8, 4, 1)
        .expect("a fresh name is claimed");

    // `b`, then `a`, then an eight-byte field that did not exist.
    let plan = FieldPlan::between(
        &[
            LayoutField {
                name: "a",
                type_tag: "u32",
                offset: 0,
                size: 4,
            },
            LayoutField {
                name: "b",
                type_tag: "u32",
                offset: 4,
                size: 4,
            },
        ],
        &[
            LayoutField {
                name: "b",
                type_tag: "u32",
                offset: 0,
                size: 4,
            },
            LayoutField {
                name: "a",
                type_tag: "u32",
                offset: 4,
                size: 4,
            },
            LayoutField {
                name: "added",
                type_tag: "u32",
                offset: 8,
                size: 8,
            },
        ],
    );

    // With no value stored, the declaration moves and nothing is migrated.
    assert_eq!(
        world
            .relayout_foreign_resource(id, 16, 8, 2, &plan)
            .expect("the declaration moves"),
        0
    );
    assert_eq!(world.foreign_resource_layout(id), Some((16, 8, 2)));

    world
        .insert_foreign_resource_bytes(id, &[1, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
        .expect("the payload matches the new size");
    assert_eq!(
        world
            .relayout_foreign_resource(id, 16, 8, 2, &FieldPlan::new())
            .expect("a value is there to migrate"),
        1
    );
    assert_eq!(
        world.foreign_resource_bytes(id),
        Some([0_u8; 16].as_slice()),
        "an empty plan zeroes the value it migrates"
    );
}

/// A Rust value's shape is its type, so a foreign declaration cannot
/// reshape it, and a plan that does not fit the payload is refused before
/// anything moves.
#[test]
fn relayout_refuses_a_rust_value_and_a_plan_that_leaves_the_payload() {
    let mut world = World::new();
    world.register_resource::<artifact_a::Settings>();
    let id = world
        .register_foreign_resource("demo::Settings", "Project.Settings", 4, 4, 1)
        .expect("the same layout joins");

    // A Rust value stored under the id brings its own table with it, so the
    // declaration is the Rust type's from then on and a foreign relayout is
    // no longer even addressed to one.
    world.insert_resource(artifact_a::Settings { value: 3 });
    assert!(matches!(
        world.relayout_foreign_resource(id, 8, 4, 2, &FieldPlan::new()),
        Err(WorldError::ForeignResourceNotRegistered { .. })
    ));

    // Declaring it foreign again - what the managed side does on every
    // reload - is refused: the stored value's destructor is the price of
    // admitting the declaration, so it is not admitted.
    assert!(matches!(
        world.register_foreign_resource("demo::Settings", "Project.Settings", 4, 4, 1),
        Err(WorldError::ForeignResourceHoldsRustValue { .. })
    ));
    assert_eq!(
        world.foreign_resource_layout(id),
        None,
        "the Rust value's table still owns the id"
    );

    // A declaration that was foreign from the start keeps its payload
    // checks: there, only the plan is in the way of a resize.
    let fresh = world
        .register_foreign_resource("demo::Other", "Project.Other", 4, 4, 1)
        .expect("a fresh name is claimed");
    world
        .insert_foreign_resource_bytes(fresh, &3_u32.to_ne_bytes())
        .expect("the payload matches the size");
    let mut too_wide = FieldPlan::new();
    too_wide.push(2, 4, FieldSource::OldOffset(0));
    assert!(matches!(
        world.relayout_foreign_resource(fresh, 4, 4, 2, &too_wide),
        Err(WorldError::ForeignResourcePlanOutOfBounds { .. })
    ));
    assert_eq!(
        world.foreign_resource_layout(fresh),
        Some((4, 4, 1)),
        "a refused relayout leaves the declaration as it was"
    );
}

/// Re-homing leaves a foreign box's table alone: those bytes are not the
/// Rust type's to drop, even when it shares their name.
#[test]
fn rehoming_leaves_a_foreign_box_alone() {
    let mut world = World::new();
    let id = world
        .register_foreign_resource("demo::Settings", "Project.Settings", 4, 4, 1)
        .expect("a fresh name is claimed");
    world
        .insert_foreign_resource_bytes(id, &9_u32.to_ne_bytes())
        .expect("the payload matches the size");

    // The Rust owner registers the same name, which makes the declaration
    // Rust's while the stored bytes stay foreign - the split a re-home has
    // to respect. Without the skip it would hand these bytes the Rust
    // type's destructor, freeing memory that type never allocated.
    world.register_resource::<artifact_a::Settings>();
    assert!(world.resources[&id].is_foreign());
    assert_eq!(
        world.resource_factories.get(&id).map(|ops| ops.foreign),
        Some(false),
        "the Rust registration owns the declaration again"
    );

    world.rehome_resources();

    assert!(
        world.resources[&id].is_foreign(),
        "a foreign box keeps its own drop, whatever the factories say"
    );
    assert_eq!(
        world.resources[&id].bytes(),
        9_u32.to_ne_bytes().as_slice(),
        "leaving the table alone leaves the payload alone too"
    );
}

/// A foreign declaration cannot take over an id that stores a Rust value:
/// the value's destructor is bound to its type, so its bytes are never
/// reinterpreted - and the table that would have dropped nothing stays out.
#[test]
fn foreign_registration_refuses_a_stored_rust_value() {
    let mut world = World::new();
    world.register_resource::<artifact_a::Settings>();
    world.insert_resource(artifact_a::Settings { value: 7 });
    let id = crate::resource::ResourceId::of::<artifact_a::Settings>();

    assert!(matches!(
        world.register_foreign_resource("demo::Settings", "Project.Settings", 4, 4, 1),
        Err(WorldError::ForeignResourceHoldsRustValue { .. })
    ));
    assert_eq!(
        world.resource_factories.get(&id).map(|ops| ops.foreign),
        Some(false),
        "the Rust registration still owns the id"
    );
    assert_eq!(
        world
            .get_resource::<artifact_a::Settings>()
            .map(|v| v.value),
        Some(7),
        "the refusal left the value alone"
    );
}

/// Byte views are for foreign payloads only: a Rust value that declares a
/// shared name is not served as bytes, and the refusal costs nothing - no
/// tick is stamped and no payload is displaced.
#[test]
fn foreign_byte_views_refuse_a_rust_owned_resource() {
    let mut world = World::new();
    world.register_resource::<artifact_a::Settings>();
    world.insert_resource(artifact_a::Settings { value: 5 });
    let id = crate::resource::ResourceId::of::<artifact_a::Settings>();

    assert!(world.foreign_resource_bytes(id).is_none());
    assert!(world.foreign_resource_bytes_mut(id).is_none());
    assert!(matches!(
        world.insert_foreign_resource_bytes(id, &5_u32.to_ne_bytes()),
        Err(WorldError::ForeignResourceFactoryIsNative { .. })
    ));
    assert_eq!(
        world
            .get_resource::<artifact_a::Settings>()
            .map(|v| v.value),
        Some(5),
        "the value survived the refusals"
    );
}

/// A zero-sized Rust type is a component like any other.
///
/// This is the tag/marker idiom (`struct Enemy;`), and the unification broke it:
/// the native registration path fed the descriptor lane's zero-width refusal
/// into an `.expect`, so declaring a marker aborted the process. No test
/// covered the case, which is why every gate stayed green through the change.
#[test]
fn a_zero_sized_component_lives_a_full_life() {
    #[derive(Clone, Debug, Default)]
    struct Enemy;
    impl Component for Enemy {}

    #[derive(Clone, Debug, Default)]
    struct Health {
        points: u32,
    }
    impl Component for Health {}

    let mut world = World::new();
    world.register_component::<Enemy>();
    world.register_component::<Health>();

    // Spawn: a marker alone, and a marker beside a sized component, so the
    // zero-sized column is exercised both as an archetype's only column and as
    // one of several.
    let lone = world.create_entity().with(Enemy).build().unwrap();
    let paired = world
        .create_entity()
        .with(Enemy)
        .with(Health { points: 7 })
        .build()
        .unwrap();

    assert!(world.get_component::<Enemy>(lone).is_some());
    assert!(world.get_component::<Enemy>(paired).is_some());

    // Column contents: a marker is a filter, which is the whole reason to
    // declare one, so both rows have to be in the column the filter reads.
    let marker = ComponentId::of::<Enemy>();
    let rows: usize = world
        .archetypes
        .values()
        .filter_map(|archetype| archetype.component_storages.get(marker))
        .map(|column| column.len())
        .sum();
    assert_eq!(
        rows, 2,
        "both markers occupy a row in the zero-sized column"
    );

    // Archetype move: adding a component migrates every column, the zero-sized
    // one included, and its rows have no bytes to carry.
    world
        .add_component(lone, Health { points: 3 })
        .expect("the marker's entity accepts another component");
    assert!(world.get_component::<Enemy>(lone).is_some());
    assert_eq!(world.get_component::<Health>(lone).unwrap().points, 3);

    // Removal: the swap-remove that closes the gap copies zero bytes, and the
    // surviving row must still be found.
    world
        .remove_component::<Enemy>(paired)
        .expect("the marker comes off");
    assert!(world.get_component::<Enemy>(paired).is_none());
    assert!(
        world.get_component::<Enemy>(lone).is_some(),
        "removing one marker leaves the other"
    );
    assert_eq!(
        world.get_component::<Health>(paired).unwrap().points,
        7,
        "the sized companion is untouched by the marker's removal"
    );

    // Destruction: the column frees a buffer it never allocated.
    assert!(world.destroy_entity(lone), "the entity is destroyed");
    assert!(world.get_component::<Enemy>(lone).is_none());
}

/// A zero-sized component with a wider alignment is still addressable.
///
/// A column that never allocates keeps its dangling pointer for life, so that
/// pointer has to be aligned for the element rather than for `u8`. Reading a
/// row through a misaligned pointer is undefined behaviour even when the row
/// has no bytes, so this pins the alignment rather than the size.
#[test]
fn an_over_aligned_zero_sized_component_is_addressable() {
    #[derive(Clone, Debug, Default)]
    #[repr(align(16))]
    struct AlignedTag;
    impl Component for AlignedTag {}

    assert_eq!(std::mem::size_of::<AlignedTag>(), 0);
    assert_eq!(std::mem::align_of::<AlignedTag>(), 16);

    let mut world = World::new();
    world.register_component::<AlignedTag>();
    let entity = world.create_entity().with(AlignedTag).build().unwrap();

    assert!(world.get_component::<AlignedTag>(entity).is_some());
}

/// A zero-width *descriptor* declaration stays refused.
///
/// The verdict is split, not loosened: a descriptor's width arrives from a
/// manifest another language wrote, where zero means that declaration is wrong.
/// Pinned so a later change cannot quietly collapse the two rules back into
/// one and start accepting a malformed manifest.
#[test]
fn a_zero_width_descriptor_component_is_still_refused() {
    let mut world = World::new();

    let refused = world.register_component_descriptor(
        0x2E_0001,
        "probe::ZeroWidth".to_string(),
        0,
        4,
        11,
        Blittability::from_manifest_fields(),
    );

    assert!(
        matches!(refused, Err(WorldError::DescriptorSizeZero)),
        "a zero-width manifest declaration is a declaration error, not a marker"
    );
}

/// A foreign resource's declared fields are stored and served back.
///
/// Without this a foreign resource is opaque bytes: the editor has nothing
/// to name, and neither does anything else that wants more than a width.
#[test]
fn a_foreign_resource_serves_its_declared_field_layout() {
    let mut world = World::new();
    let id = world
        .register_foreign_resource("probe::Tuning", "Probe.Tuning", 8, 4, 1)
        .expect("a valid foreign layout registers");

    assert!(
        world.resource_field_layout(id).is_none(),
        "a resource registered without a layout is simply not inspectable"
    );

    world
        .register_resource_field_layout(
            id,
            vec![
                crate::component_registry::ComponentFieldDescriptor {
                    name: "speed",
                    type_tag: "f32",
                    offset: 0,
                    size: 4,
                    align: 4,
                    element_count: 0,
                },
                crate::component_registry::ComponentFieldDescriptor {
                    name: "gain",
                    type_tag: "f32",
                    offset: 4,
                    size: 4,
                    align: 4,
                    element_count: 0,
                },
            ],
        )
        .expect("the fields fit the registered size");

    let fields = world
        .resource_field_layout(id)
        .expect("the layout is stored");
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].name, "speed");
    assert_eq!(fields[1].offset, 4);
}

/// A field layout that cannot describe the resource is refused.
///
/// Both checks the descriptor-component path applies, for the same reason:
/// a foreign resource's bytes are as opaque as a descriptor row, so a
/// reader has to be kept inside the value and away from any pointer pair
/// the bytes never held.
#[test]
fn an_impossible_resource_field_layout_is_refused() {
    let mut world = World::new();
    let id = world
        .register_foreign_resource("probe::Narrow", "Probe.Narrow", 4, 4, 1)
        .expect("a valid foreign layout registers");

    let past_the_end = world.register_resource_field_layout(
        id,
        vec![crate::component_registry::ComponentFieldDescriptor {
            name: "wide",
            type_tag: "u64",
            offset: 0,
            size: 8,
            align: 8,
            element_count: 0,
        }],
    );
    assert!(
        past_the_end.is_err(),
        "a field may not reach past the value"
    );

    let container = world.register_resource_field_layout(
        id,
        vec![crate::component_registry::ComponentFieldDescriptor {
            name: "items",
            type_tag: "vec:f32",
            offset: 0,
            size: 4,
            align: 4,
            element_count: 0,
        }],
    );
    assert!(
        container.is_err(),
        "a container tag would be read as a pointer and length the bytes do not hold"
    );
}

/// A relayout drops the layout it described.
///
/// Serving the previous generation's offsets over migrated bytes would be
/// worse than serving nothing, so the engine forgets and the declarer
/// re-publishes - which is what the host does on the same pass.
#[test]
fn a_relayout_drops_the_stored_resource_field_layout() {
    let mut world = World::new();
    let id = world
        .register_foreign_resource("probe::Moving", "Probe.Moving", 4, 4, 1)
        .expect("a valid foreign layout registers");
    world
        .register_resource_field_layout(
            id,
            vec![crate::component_registry::ComponentFieldDescriptor {
                name: "value",
                type_tag: "u32",
                offset: 0,
                size: 4,
                align: 4,
                element_count: 0,
            }],
        )
        .expect("the field fits");
    assert!(world.resource_field_layout(id).is_some());

    let plan = FieldPlan::between(
        &[LayoutField {
            name: "value",
            type_tag: "u32",
            offset: 0,
            size: 4,
        }],
        &[
            LayoutField {
                name: "value",
                type_tag: "u32",
                offset: 0,
                size: 4,
            },
            LayoutField {
                name: "added",
                type_tag: "u32",
                offset: 4,
                size: 4,
            },
        ],
    );
    world
        .relayout_foreign_resource(id, 8, 4, 2, &plan)
        .expect("the wider shape is valid");

    assert!(
        world.resource_field_layout(id).is_none(),
        "the layout that described the old shape is forgotten"
    );
}
