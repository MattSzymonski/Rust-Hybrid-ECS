//! Nested `#[derive(PillMirror)]` fields of a derived component.
//!
//! # Responsibilities
//!
//! - Show that a component registered through `register_all_components`
//!   expands a field of a mirrored value type into dotted leaf rows: the
//!   facade hands the engine core this artifact's value types for the loop.
//! - Show that the value types are handed over only for that loop, so nothing
//!   outside it resolves through them.

use pill_engine::component_registry::{register_all_components, ComponentFieldDescriptor};
use pill_engine::{Component, ComponentId, PillComponent, PillMirror, World};

/// A mirrored value type, known to the engine only through this artifact's
/// value-type registry.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PillMirror)]
struct NestedVector {
    x: f32,
    y: f32,
}

/// A derived component with one scalar and one value-type field.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PillComponent)]
struct Carrier {
    lead: f32,
    offset: NestedVector,
}

/// The derive's registration, run by the artifact-wide loop, expands the
/// value-type field channel by channel with composed offsets.
#[test]
fn a_value_type_field_expands_when_registered_through_the_loop() {
    let mut world = World::new();
    register_all_components(&mut world).expect("registration succeeds");

    let layout = world
        .component_field_layout(ComponentId::of::<Carrier>())
        .expect("the derive registers a field layout");
    let rows: Vec<(&str, usize)> = layout
        .iter()
        .map(|field| (field.name, field.offset))
        .collect();
    assert_eq!(rows, [("lead", 0), ("offset.x", 4), ("offset.y", 8)]);
}

/// A component registered by hand after the loop returns sees no value types:
/// the loop restored the world's previous (empty) resolver.
#[test]
fn the_value_types_are_handed_over_only_for_the_loop() {
    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    struct HandRegistered {
        offset: NestedVector,
    }
    impl Component for HandRegistered {}

    static FIELDS: &[ComponentFieldDescriptor] = &[ComponentFieldDescriptor {
        name: "offset",
        type_tag: "struct:NestedVector",
        offset: 0,
        size: 8,
        align: 4,
        element_count: 0,
    }];

    let mut world = World::new();
    register_all_components(&mut world).expect("registration succeeds");
    world.register_component_with_layout::<HandRegistered>(FIELDS);

    let layout = world
        .component_field_layout(ComponentId::of::<HandRegistered>())
        .expect("registered with a layout");
    assert_eq!(
        layout.len(),
        1,
        "the field stays one opaque row: {layout:?}"
    );
    assert_eq!(layout[0].type_tag, "struct:NestedVector");
}
