//! Integration tests for heap-owning component fields.
//!
//! Covers the container pipeline end to end: the derive's registered tag
//! vocabulary, the generated accessor trampolines reached through a raw row
//! address, and the reload-migration behavior that heap data depends on —
//! serde round-tripping a changed schema while an unchanged one keeps the live
//! allocation in place.

use std::collections::HashSet;

use pill_engine::archetype::Blittability;
use pill_engine::component_registry::{register_all_components, ComponentFieldDescriptor};
use pill_engine::{ComponentId, PillComponent, World};

/// A dynamic component's registered layout is validated: a descriptor that
/// reaches past the row or claims a container tag is refused, and the
/// previously registered layout is left in place.
#[test]
fn an_overflowing_field_layout_is_refused() {
    let mut world = World::new();
    // SAFETY: this row is eight bytes the test never reads as anything but
    // bytes; no pointer or owner is involved.
    let witness = unsafe { Blittability::assume() };
    let component_id = world
        .register_dynamic_component(0x51, "Heap.LayoutProbe", 8, 4, 1, witness)
        .expect("the component registers");

    let good = vec![ComponentFieldDescriptor {
        name: "value",
        type_tag: "u32",
        offset: 0,
        size: 4,
        align: 4,
        element_count: 0,
    }];
    world
        .register_dynamic_component_field_layout(component_id, good.clone())
        .expect("a fitting layout is accepted");

    let overflow = vec![ComponentFieldDescriptor {
        name: "whole",
        type_tag: "u32",
        offset: usize::MAX,
        size: 4,
        align: 4,
        element_count: 0,
    }];
    assert!(
        world
            .register_dynamic_component_field_layout(component_id, overflow)
            .is_err(),
        "an overflowing field range is refused"
    );

    let container = vec![ComponentFieldDescriptor {
        name: "text",
        type_tag: "string",
        offset: 0,
        size: 8,
        align: 4,
        element_count: 0,
    }];
    assert!(
        world
            .register_dynamic_component_field_layout(component_id, container)
            .is_err(),
        "a container tag on a dynamic row is refused"
    );

    // Both refusals left the accepted layout in place.
    assert_eq!(
        world.component_field_layout(component_id),
        Some(good.as_slice())
    );
}

/// A persistable component mixing both container kinds with an inline scalar,
/// the way real gameplay data does.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PillComponent)]
#[pill(persistable)]
struct HeapProbe {
    /// Heap-owning sequence field.
    values: Vec<f32>,
    /// Heap-owning text field.
    label: String,
    /// Inline field, pinning that blittable fields still mirror normally.
    weight: u32,
}

/// The derive's own layout for [`HeapProbe`], spelled out so a test can watch
/// the hash move when only a container tag differs.
static DERIVE_LAYOUT: &[ComponentFieldDescriptor] = &[
    ComponentFieldDescriptor {
        name: "values",
        type_tag: "vec:f32",
        offset: 0,
        size: 24,
        align: 8,
        element_count: 0,
    },
    ComponentFieldDescriptor {
        name: "label",
        type_tag: "string",
        offset: 24,
        size: 24,
        align: 8,
        element_count: 0,
    },
    ComponentFieldDescriptor {
        name: "weight",
        type_tag: "u32",
        offset: 48,
        size: 4,
        align: 4,
        element_count: 0,
    },
];

/// A deliberately altered layout for the same Rust type, standing in for a
/// build where the declared shape moved; used to force the migration path.
static ALTERED_LAYOUT: &[ComponentFieldDescriptor] = &[
    ComponentFieldDescriptor {
        name: "values",
        type_tag: "vec:f64",
        offset: 0,
        size: 24,
        align: 8,
        element_count: 0,
    },
    ComponentFieldDescriptor {
        name: "label",
        type_tag: "string",
        offset: 24,
        size: 24,
        align: 8,
        element_count: 0,
    },
    ComponentFieldDescriptor {
        name: "weight",
        type_tag: "u32",
        offset: 48,
        size: 4,
        align: 4,
        element_count: 0,
    },
];

/// The collision case the layout-aware hash exists for: same size and same
/// default JSON shape, different element kind.
static STRING_ELEMENT_LAYOUT: &[ComponentFieldDescriptor] = &[
    ComponentFieldDescriptor {
        name: "values",
        type_tag: "vec:string",
        offset: 0,
        size: 24,
        align: 8,
        element_count: 0,
    },
    ComponentFieldDescriptor {
        name: "label",
        type_tag: "string",
        offset: 24,
        size: 24,
        align: 8,
        element_count: 0,
    },
    ComponentFieldDescriptor {
        name: "weight",
        type_tag: "u32",
        offset: 48,
        size: 4,
        align: 4,
        element_count: 0,
    },
];

/// A persistable component whose list holds individually allocated strings,
/// the shape that has no element span and needs per-element accessors.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PillComponent)]
#[pill(persistable)]
struct NameList {
    /// Text list: each element is its own allocation.
    names: Vec<String>,
    /// Inline field after the container.
    weight: u32,
}

/// The derive's layout for [`NameList`].
static NAME_LAYOUT: &[ComponentFieldDescriptor] = &[
    ComponentFieldDescriptor {
        name: "names",
        type_tag: "vec:string",
        offset: 0,
        size: 24,
        align: 8,
        element_count: 0,
    },
    ComponentFieldDescriptor {
        name: "weight",
        type_tag: "u32",
        offset: 24,
        size: 4,
        align: 4,
        element_count: 0,
    },
];

/// A deliberately altered layout for [`NameList`]: the declared element kind
/// moved, which forces the migration path on the next reload.
static NAME_ALTERED_LAYOUT: &[ComponentFieldDescriptor] = &[
    ComponentFieldDescriptor {
        name: "names",
        type_tag: "vec:f32",
        offset: 0,
        size: 24,
        align: 8,
        element_count: 0,
    },
    ComponentFieldDescriptor {
        name: "weight",
        type_tag: "u32",
        offset: 24,
        size: 4,
        align: 4,
        element_count: 0,
    },
];

/// The derive registers container fields with their element-carrying tags, so
/// the C# codegen and the schema hash both see the real shape.
#[test]
fn derive_registers_container_tags() {
    let mut world = World::new();
    register_all_components(&mut world).expect("registration succeeds");

    let layout = world
        .component_field_layout(ComponentId::of::<HeapProbe>())
        .expect("the derive registers a field layout");
    let tag = |name: &str| {
        layout
            .iter()
            .find(|field| field.name == name)
            .map(|field| field.type_tag)
    };
    assert_eq!(tag("values"), Some("vec:f32"));
    assert_eq!(tag("label"), Some("string"));
    assert_eq!(tag("weight"), Some("u32"));
}

/// The generated view/resize/set trampolines reach the live value: a view
/// answers the real buffer, a resize grows the real `Vec`, and a set replaces
/// the real `String` while refusing invalid UTF-8.
#[test]
fn accessor_trampolines_operate_on_the_live_value() {
    let mut probe = HeapProbe {
        values: vec![1.0, 2.5],
        label: "before".to_string(),
        weight: 7,
    };
    // A raw address rather than a reference, so nothing borrows `probe` while
    // the trampolines mutate it.
    let row = std::ptr::addr_of_mut!(probe).cast::<u8>();

    // SAFETY: `row` addresses the live `HeapProbe` above for the whole test,
    // and every out pointer is a valid local the trampoline may write.
    unsafe {
        let mut data: *const u8 = std::ptr::null();
        let mut length = 0usize;
        assert_eq!(
            pill_accessor_HeapProbe_values_view(row as *const u8, &mut data, &mut length),
            0
        );
        assert_eq!(length, 2);
        assert_eq!(
            std::slice::from_raw_parts(data as *const f32, length),
            &[1.0, 2.5]
        );

        // Growing fills the new elements with the element type's default.
        assert_eq!(pill_accessor_HeapProbe_values_resize(row, 4), 0);
        assert_eq!(
            pill_accessor_HeapProbe_values_view(row as *const u8, &mut data, &mut length),
            0
        );
        assert_eq!(length, 4);
        assert_eq!(
            std::slice::from_raw_parts(data as *const f32, length),
            &[1.0, 2.5, 0.0, 0.0]
        );

        // Text: a valid replacement lands, invalid UTF-8 is refused.
        let replacement = "after";
        assert_eq!(
            pill_accessor_HeapProbe_label_set(row, replacement.as_ptr(), replacement.len()),
            0
        );
        assert_eq!(
            pill_accessor_HeapProbe_label_view(row as *const u8, &mut data, &mut length),
            0
        );
        assert_eq!(
            std::slice::from_raw_parts(data, length),
            replacement.as_bytes()
        );
        assert_eq!(
            pill_accessor_HeapProbe_label_set(row, [0xFFu8].as_ptr(), 1),
            2
        );
    }

    assert_eq!(probe.values, vec![1.0, 2.5, 0.0, 0.0]);
    assert_eq!(probe.label, "after");
}

/// An element-kind change alone reaches the schema hash, so a reload migrates
/// the values instead of keeping the old buffer and reinterpreting it.
#[test]
fn container_element_kind_changes_the_schema_hash() {
    let mut world = World::new();
    world.register_persistable_component_with_layout::<HeapProbe>(DERIVE_LAYOUT);
    let hash_with_float_elements = schema_hash(&world);

    world.register_persistable_component_with_layout::<HeapProbe>(STRING_ELEMENT_LAYOUT);
    let hash_with_string_elements = schema_hash(&world);

    assert_ne!(
        hash_with_float_elements, hash_with_string_elements,
        "a container element-kind change must change the schema hash"
    );
}

/// A changed schema migrates heap contents through serde; an unchanged one
/// leaves the live allocation exactly where it was.
#[test]
fn heap_fields_migrate_with_serde_and_survive_the_fast_path() {
    let mut world = World::new();
    world.register_persistable_component_with_layout::<HeapProbe>(DERIVE_LAYOUT);
    let entity = world
        .create_entity()
        .with(HeapProbe {
            values: vec![3.5, 4.5],
            label: "kept".to_string(),
            weight: 9,
        })
        .build()
        .expect("entity is created");

    let type_name = world
        .persist_type_manifest()
        .into_iter()
        .map(|entry| entry.type_name)
        .find(|name| name.ends_with("HeapProbe"))
        .expect("the component is persistable");

    // Simulate a reload whose declared shape moved: capture the retiring
    // generation's metadata, register the new shape, then migrate.
    let previous = world.capture_persist_type_metadata();
    world.register_persistable_component_with_layout::<HeapProbe>(ALTERED_LAYOUT);

    let changed: HashSet<String> = HashSet::from([type_name.clone()]);
    let report = world.migrate_changed_persistable_components(&previous, &changed, None);
    assert_eq!(report.migrated_type_count, 1);
    assert_eq!(report.migrated_entity_count, 1);

    let probe = world
        .get_component::<HeapProbe>(entity)
        .expect("data survived the migration");
    assert_eq!(probe.values, vec![3.5, 4.5]);
    assert_eq!(probe.label, "kept");
    assert_eq!(probe.weight, 9);
    let buffer = probe.values.as_ptr();

    // The unchanged-schema fast path visits no value at all: the same buffer
    // remains in place, which is what makes the common reload cheap and keeps
    // outstanding heap pointers valid.
    let previous = world.capture_persist_type_metadata();
    let report = world.migrate_changed_persistable_components(&previous, &HashSet::new(), None);
    assert_eq!(report.migrated_type_count, 0);
    let probe = world
        .get_component::<HeapProbe>(entity)
        .expect("data survived the fast path");
    assert_eq!(probe.values.as_ptr(), buffer);
    assert_eq!(probe.values, vec![3.5, 4.5]);
}

/// A `Vec<String>` field registers as `vec:string`, the tag that tells the
/// codegen its elements have no span and travel through element accessors.
#[test]
fn vec_string_field_registers_the_elementwise_tag() {
    let mut world = World::new();
    register_all_components(&mut world).expect("registration succeeds");

    let layout = world
        .component_field_layout(ComponentId::of::<NameList>())
        .expect("the derive registers a field layout");
    let names = layout
        .iter()
        .find(|field| field.name == "names")
        .expect("the list field is declared");
    assert_eq!(names.type_tag, "vec:string");
}

/// The per-element trampolines reach the live list: the view answers a count
/// and a null run, item/set/push move one string each, out-of-range and
/// invalid UTF-8 are refused, and growing fills empty strings.
#[test]
fn vec_string_trampolines_reach_each_element() {
    let mut list = NameList {
        names: vec!["alpha".to_string(), "beta".to_string()],
        weight: 3,
    };
    // A raw address rather than a reference, so nothing borrows `list` while
    // the trampolines mutate it.
    let row = std::ptr::addr_of_mut!(list).cast::<u8>();

    // SAFETY: `row` addresses the live `NameList` above for the whole test,
    // and every out pointer is a valid local the trampoline may write.
    unsafe {
        // The view reports the count only: elements are separately allocated,
        // so there is no run to point a span at, and the data pointer is null
        // no matter what the caller seeded it with.
        let mut data: *const u8 = 0x1234_usize as *const u8;
        let mut length = 0usize;
        assert_eq!(
            pill_accessor_NameList_names_view(row as *const u8, &mut data, &mut length),
            0
        );
        assert!(data.is_null());
        assert_eq!(length, 2);

        // An element read borrows one string for the call.
        assert_eq!(
            pill_accessor_NameList_names_item(row as *const u8, 1, &mut data, &mut length),
            0
        );
        assert_eq!(
            std::str::from_utf8(std::slice::from_raw_parts(data, length)).unwrap(),
            "beta"
        );
        assert_eq!(
            pill_accessor_NameList_names_item(row as *const u8, 2, &mut data, &mut length),
            2
        );

        // Replacing an element, with the two refusals: out of range, bad
        // UTF-8 - the slot must keep its old text when either fires.
        let replacement = "gamma";
        assert_eq!(
            pill_accessor_NameList_names_set_item(row, 1, replacement.as_ptr(), replacement.len()),
            0
        );
        assert_eq!(
            pill_accessor_NameList_names_set_item(row, 5, replacement.as_ptr(), replacement.len()),
            2
        );
        assert_eq!(
            pill_accessor_NameList_names_set_item(row, 0, [0xFFu8].as_ptr(), 1),
            3
        );

        // Appending, then growing: the new elements are empty strings.
        let appended = "delta";
        assert_eq!(
            pill_accessor_NameList_names_push(row, appended.as_ptr(), appended.len()),
            0
        );
        assert_eq!(pill_accessor_NameList_names_resize(row, 6), 0);
    }

    assert_eq!(list.names, vec!["alpha", "gamma", "delta", "", "", ""]);
    assert_eq!(list.weight, 3);
}

/// String lists migrate through serde when the schema changes, and stay in
/// place on the fast path - the same reload contract the other containers
/// follow, only with each element's allocation inside the serialized text.
#[test]
fn vec_string_contents_migrate_with_serde_and_survive_the_fast_path() {
    let mut world = World::new();
    world.register_persistable_component_with_layout::<NameList>(NAME_LAYOUT);
    let entity = world
        .create_entity()
        .with(NameList {
            names: vec!["one".to_string(), "two".to_string()],
            weight: 12,
        })
        .build()
        .expect("entity is created");

    let type_name = world
        .persist_type_manifest()
        .into_iter()
        .map(|entry| entry.type_name)
        .find(|name| name.ends_with("NameList"))
        .expect("the component is persistable");

    let previous = world.capture_persist_type_metadata();
    world.register_persistable_component_with_layout::<NameList>(NAME_ALTERED_LAYOUT);
    let changed: HashSet<String> = HashSet::from([type_name.clone()]);
    let report = world.migrate_changed_persistable_components(&previous, &changed, None);
    assert_eq!(report.migrated_type_count, 1);
    assert_eq!(report.migrated_entity_count, 1);

    let list = world
        .get_component::<NameList>(entity)
        .expect("data survived the migration");
    assert_eq!(list.names, vec!["one", "two"]);
    assert_eq!(list.weight, 12);
    let buffer = list.names.as_ptr();

    let previous = world.capture_persist_type_metadata();
    let report = world.migrate_changed_persistable_components(&previous, &HashSet::new(), None);
    assert_eq!(report.migrated_type_count, 0);
    let list = world
        .get_component::<NameList>(entity)
        .expect("data survived the fast path");
    assert_eq!(list.names.as_ptr(), buffer);
    assert_eq!(list.names, vec!["one", "two"]);
}

/// The schema hash of the one registered `HeapProbe`, read through the
/// manifest the reload path compares.
fn schema_hash(world: &World) -> u64 {
    world
        .persist_type_manifest()
        .into_iter()
        .find(|entry| entry.type_name.ends_with("HeapProbe"))
        .expect("the component is registered")
        .schema_hash
}
