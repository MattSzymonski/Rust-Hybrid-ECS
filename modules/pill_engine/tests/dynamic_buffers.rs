//! Integration tests for engine-owned native dynamic buffers.
//!
//! Pins the address-stability contract end to end: the derive's tag, the
//! generated trampolines, the archetype-move behavior (the handle is copied,
//! the block is not), migration through serde, the unchanged-schema fast path,
//! and balanced ownership of the engine's native memory.

use std::collections::HashSet;
use std::sync::Mutex;

use pill_core::native_buffer;
use pill_engine::component_registry::{register_all_components, ComponentFieldDescriptor};
use pill_engine::{ComponentId, DynamicBuffer, PillComponent, World};

/// A persistable component holding an engine-owned buffer plus an inline
/// scalar, so a structural change exercises both.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PillComponent)]
#[pill(persistable)]
struct Trail {
    points: DynamicBuffer<f32>,
    weight: u32,
}

/// A second component used to force an archetype move.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PillComponent)]
#[pill(persistable)]
struct Marker {
    value: u32,
}

/// The derive's own layout for [`Trail`], spelled out so the migration test
/// can move the schema hash by changing one tag.
static DERIVE_LAYOUT: &[ComponentFieldDescriptor] = &[
    ComponentFieldDescriptor {
        name: "points",
        type_tag: "dynbuf:f32",
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

/// A deliberately altered layout for the same Rust type, standing in for a
/// build whose declared shape moved.
static ALTERED_LAYOUT: &[ComponentFieldDescriptor] = &[
    ComponentFieldDescriptor {
        name: "points",
        type_tag: "dynbuf:f64",
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

/// Serializes the tests that assert on the process-wide block accounting.
static ACCOUNTING_GUARD: Mutex<()> = Mutex::new(());

fn accounted() -> std::sync::MutexGuard<'static, ()> {
    ACCOUNTING_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The derive registers an engine-owned buffer with its `dynbuf:` tag, which
/// is what the C# codegen and the schema hash both read.
#[test]
fn derive_registers_the_dynbuf_tag() {
    let _guard = accounted();
    let mut world = World::new();
    register_all_components(&mut world).expect("registration succeeds");

    let layout = world
        .component_field_layout(ComponentId::of::<Trail>())
        .expect("the derive registers a field layout");
    let tag = |name: &str| {
        layout
            .iter()
            .find(|field| field.name == name)
            .map(|field| field.type_tag)
    };
    assert_eq!(tag("points"), Some("dynbuf:f32"));
    assert_eq!(tag("weight"), Some("u32"));
}

/// The generated trampolines reach the live handle: a view answers the real
/// block and a resize rewrites it in place.
#[test]
fn accessor_trampolines_resize_the_engine_owned_buffer() {
    let _guard = accounted();
    let mut trail = Trail {
        points: DynamicBuffer::from_slice(&[1.0, 2.0]),
        weight: 3,
    };
    // A raw address rather than a reference, so nothing borrows `trail` while
    // the trampolines mutate it.
    let row = std::ptr::addr_of_mut!(trail).cast::<u8>();

    // SAFETY: `row` addresses the live `Trail` above for the whole test, and
    // every out pointer is a valid local the trampoline may write.
    unsafe {
        let mut data: *const u8 = std::ptr::null();
        let mut length = 0usize;
        assert_eq!(
            pill_accessor_Trail_points_view(row as *const u8, &mut data, &mut length),
            0
        );
        assert_eq!(length, 2);
        assert_eq!(
            std::slice::from_raw_parts(data as *const f32, length),
            &[1.0, 2.0]
        );

        // Growing fills the new elements with the element type's default.
        assert_eq!(pill_accessor_Trail_points_resize(row, 4), 0);
        assert_eq!(
            pill_accessor_Trail_points_view(row as *const u8, &mut data, &mut length),
            0
        );
        assert_eq!(length, 4);
        assert_eq!(
            std::slice::from_raw_parts(data as *const f32, length),
            &[1.0, 2.0, 0.0, 0.0]
        );
    }

    assert_eq!(trail.points.as_slice(), &[1.0, 2.0, 0.0, 0.0]);
}

/// A structural change copies the handle and never the block: the address an
/// element view was taken from stays valid across an archetype move, which is
/// the property generated C# spans rely on.
#[test]
fn archetype_move_copies_the_handle_not_the_block() {
    let _guard = accounted();
    let mut world = World::new();
    register_all_components(&mut world).expect("registration succeeds");
    let entity = world
        .create_entity()
        .with(Trail {
            points: DynamicBuffer::from_slice(&[7.0, 8.0, 9.0]),
            weight: 1,
        })
        .build()
        .expect("entity is created");

    let before = world
        .get_component::<Trail>(entity)
        .expect("trail is present");
    let address = before.points.as_ptr();
    let bytes = native_buffer::live_bytes();

    // Adding a component moves the entity into a new archetype, which copies
    // every surviving component through its registered copier.
    world
        .add_component::<Marker>(entity, Marker { value: 5 })
        .expect("component is added");

    let after = world
        .get_component::<Trail>(entity)
        .expect("trail survived the move");
    assert_eq!(
        after.points.as_ptr(),
        address,
        "the block must not move when the entity does"
    );
    assert_eq!(after.points.as_slice(), &[7.0, 8.0, 9.0]);
    assert_eq!(
        native_buffer::live_bytes(),
        bytes,
        "a handle copy must not allocate or leak a block"
    );

    drop(world);
    assert_eq!(
        native_buffer::live_bytes(),
        0,
        "dropping the world must release every block"
    );
}

/// A changed schema migrates the buffer's contents through serde; an unchanged
/// one leaves the block - and its address - exactly where it was.
#[test]
fn buffer_contents_survive_migration_and_the_fast_path() {
    let _guard = accounted();
    let mut world = World::new();
    world.register_persistable_component_with_layout::<Trail>(DERIVE_LAYOUT);
    let entity = world
        .create_entity()
        .with(Trail {
            points: DynamicBuffer::from_slice(&[3.5, 4.5]),
            weight: 9,
        })
        .build()
        .expect("entity is created");

    let type_name = world
        .persist_type_manifest()
        .into_iter()
        .map(|entry| entry.type_name)
        .find(|name| name.ends_with("Trail"))
        .expect("the component is persistable");

    // Simulate a reload whose declared shape moved: capture the retiring
    // generation's metadata, register the new shape, then migrate.
    let previous = world.capture_persist_type_metadata();
    world.register_persistable_component_with_layout::<Trail>(ALTERED_LAYOUT);
    let changed: HashSet<String> = HashSet::from([type_name]);
    let report = world.migrate_changed_persistable_components(&previous, &changed);
    assert_eq!(report.migrated_type_count, 1);
    assert_eq!(report.migrated_entity_count, 1);

    let trail = world
        .get_component::<Trail>(entity)
        .expect("data survived the migration");
    assert_eq!(trail.points.as_slice(), &[3.5, 4.5]);
    assert_eq!(trail.weight, 9);
    let address = trail.points.as_ptr();

    // The unchanged-schema fast path visits no value at all: the same block
    // stays in place, which is what keeps outstanding views valid.
    let previous = world.capture_persist_type_metadata();
    let report = world.migrate_changed_persistable_components(&previous, &HashSet::new());
    assert_eq!(report.migrated_type_count, 0);
    let trail = world
        .get_component::<Trail>(entity)
        .expect("data survived the fast path");
    assert_eq!(trail.points.as_ptr(), address);
    assert_eq!(trail.points.as_slice(), &[3.5, 4.5]);

    drop(world);
    assert_eq!(native_buffer::live_bytes(), 0);
}
