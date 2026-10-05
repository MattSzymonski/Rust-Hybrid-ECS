//! Compile-time field layouts of components and value types.
//!
//! # Responsibilities
//!
//! - Declare
//!   [`ComponentFieldDescriptor`](crate::component_registry::ComponentFieldDescriptor),
//!   the field layout the derive macros emit and the world stores for the
//!   editor and the C# mirror codegen.
//! - Declare the
//!   [`ComponentLayout`](crate::component_registry::ComponentLayout) trait the
//!   derives implement.
//! - Declare
//!   [`ValueTypeLayoutResolver`](crate::component_registry::ValueTypeLayoutResolver),
//!   through which the world finds the layout of a value type a component
//!   field nests.
//!
//! # Design
//!
//! The registries the derives submit into (`PillComponentDescriptor` and the
//! other descriptor types) live in `pill_engine`, the facade crate every
//! project and extension depends on, not here. The facade is compiled into
//! every DLL, so each DLL's registries hold exactly what that DLL declared;
//! this crate holds only what is the same for all of them.

/// One named field of a `#[derive(PillComponent)]` component or a
/// `#[derive(PillMirror)]` value type, captured at compile time so the host
/// can emit a typed C# mirror instead of an opaque ABI blob.
///
/// Every value is const-constructible (`offset_of!`/`size_of!`/`align_of!`),
/// so a descriptor array can live in a static inside the declaring artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ComponentFieldDescriptor {
    /// Rust field name (snake_case); the C# codegen maps it to PascalCase.
    pub name: &'static str,
    /// Type tag from a closed vocabulary — `f32`, `u32`, `bool`, ...
    /// `array:<inner>`, `struct:<path>`, `vec:<element>`, `dynbuf:<element>`,
    /// `string` — that the C# codegen maps to a concrete C# type. See
    /// `pill_host/src/csharp/codegen.rs`.
    ///
    /// `vec:<element>` and `string` describe Rust-owned heap fields, which
    /// carry no C# field of their own: managed code reaches them through the
    /// accessor members the codegen emits, whose trampolines are declared with
    /// `pill_engine::component_registry::PillFieldAccessorDescriptor`.
    /// `dynbuf:<element>` describes an engine-owned native buffer, whose
    /// `(ptr, len, cap)` handle is mirrorable as plain words — managed code
    /// reads it in place, and only resizing goes through an accessor.
    pub type_tag: &'static str,
    /// Byte offset of the field within the type (`core::mem::offset_of!`).
    pub offset: usize,
    /// Byte size of the field's type.
    pub size: usize,
    /// Byte alignment of the field's type.
    pub align: usize,
    /// Number of elements for an `array:` field; zero for non-array fields.
    /// Computed from the array length expression at compile time, so const
    /// and literal lengths alike resolve here.
    pub element_count: usize,
}

/// The compile-time field layout of a type, as the derive macros emit it.
///
/// Implemented by `#[derive(PillLayout)]`, `#[derive(PillComponent)]` and
/// `#[derive(PillMirror)]`; lets a layout reach
/// [`World::register_component_with_layout`] without restating the
/// descriptors. The shorthand for the same list is the type's inherent
/// `FIELD_LAYOUT` const.
///
/// [`World::register_component_with_layout`]: crate::World::register_component_with_layout
pub trait ComponentLayout {
    /// The declared field list, in declaration order.
    const FIELDS: &'static [ComponentFieldDescriptor];
}

/// Finds the field layout of a `#[derive(PillMirror)]` value type by the path
/// a `struct:<path>` field tag names: an exact type name first, then a unique
/// suffix match.
///
/// The value types are a per-DLL registry, which this crate cannot read. The
/// facade's `register_all_components` installs its own DLL's resolver into the
/// world for the duration of its registration loop
/// ([`World::replace_value_type_layout_resolver`]), so a component's nested
/// value-type fields expand against the value types its own DLL declares.
///
/// [`World::replace_value_type_layout_resolver`]: crate::World::replace_value_type_layout_resolver
pub type ValueTypeLayoutResolver = fn(&str) -> Option<&'static [ComponentFieldDescriptor]>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Component, ComponentId, World};

    #[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
    struct TestPersistableComponent {
        value: u32,
    }
    impl Component for TestPersistableComponent {}

    #[derive(Clone, Debug)]
    struct TestPlainComponent;
    impl Component for TestPlainComponent {}

    /// A compile-time field layout registered with a component is retrievable,
    /// while a component registered without one reports no layout.
    #[test]
    fn field_layouts_are_stored_and_queryable() {
        static FIELDS: &[ComponentFieldDescriptor] = &[ComponentFieldDescriptor {
            name: "value",
            type_tag: "u32",
            offset: 0,
            size: 4,
            align: 4,
            element_count: 0,
        }];

        let mut with_layout = World::new();
        with_layout.register_component_with_layout::<TestPlainComponent>(FIELDS);
        assert_eq!(
            with_layout.component_field_layout(ComponentId::of::<TestPlainComponent>()),
            Some(FIELDS)
        );

        let mut without_layout = World::new();
        without_layout.register_component::<TestPlainComponent>();
        assert!(without_layout
            .component_field_layout(ComponentId::of::<TestPlainComponent>())
            .is_none());
    }

    /// The persistable with-layout variant stores the layout after the
    /// standard persistable registration.
    #[test]
    fn persistable_field_layout_is_stored() {
        static FIELDS: &[ComponentFieldDescriptor] = &[ComponentFieldDescriptor {
            name: "value",
            type_tag: "u32",
            offset: 0,
            size: 4,
            align: 4,
            element_count: 0,
        }];

        let mut world = World::new();
        world.register_persistable_component_with_layout::<TestPersistableComponent>(FIELDS);
        assert_eq!(
            world.component_field_layout(ComponentId::of::<TestPersistableComponent>()),
            Some(FIELDS)
        );
    }

    /// Registering the same type first as plain, then as persistable, stays
    /// idempotent: one registry entry, one bit. This pins the invariant the
    /// unified per-type registration (audit 4.2) must preserve.
    #[test]
    fn plain_then_persistable_registration_stays_idempotent() {
        let mut world = World::new();
        world.register_component::<TestPersistableComponent>();
        let bit_before = world
            .component_registry
            .get_bit(&ComponentId::of::<TestPersistableComponent>());
        assert!(bit_before.is_some(), "plain registration must assign a bit");

        world.register_persistable_component::<TestPersistableComponent>();
        let bit_after = world
            .component_registry
            .get_bit(&ComponentId::of::<TestPersistableComponent>());
        assert_eq!(
            bit_before, bit_after,
            "persistable re-registration must reuse the existing bit"
        );

        let entry_count = world
            .component_registry
            .registered_components()
            .filter(|(id, _, _)| *id == ComponentId::of::<TestPersistableComponent>())
            .count();
        assert_eq!(
            entry_count, 1,
            "one logical type must occupy exactly one registry entry"
        );
    }
}
