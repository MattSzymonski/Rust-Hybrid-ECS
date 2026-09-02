//! Compile-time component registry driven by `#[derive(PillComponent)]`.
//!
//! # Responsibilities
//!
//! - Declare the `PillComponentDescriptor` type that the derive macro submits
//!   into the [`inventory`] collection.
//! - Provide the artifact-wide registration loop and the aggregate schema
//!   fingerprint that the module/project entry-point macros call.
//!
//! # Design
//!
//! The registry is per linked artifact: every binary and every hot-reload
//! generation DLL carries its own collection containing exactly the components
//! its own sources declared with `#[derive(PillComponent)]`. This is what makes
//! the collection safe across hot reload — a new generation's `init` sees only
//! the new generation's components, so a type that a newer build stops declaring
//! is never re-registered from a stale copy, and a generation DLL that is
//! evicted takes its descriptors with it (nothing else references them).
//!
//! Registration order is unspecified (linker/initializer order), which is
//! harmless: component registration is keyed by `TypeId` and idempotent.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::error::WorldError;
use crate::World;

/// One component type declared with `#[derive(PillComponent)]`.
///
/// Submitted into this artifact's registry by the derive macro; every field is
/// const-constructible so the descriptor can live in a static.
pub struct PillComponentDescriptor {
    /// Fully-qualified type name, as `std::any::type_name` would report it.
    pub type_name: &'static str,
    /// Whether the component is persistable (schema-migrated across reloads).
    pub persistable: bool,
    /// Compile-time field layout used by the C# mirror codegen; empty for
    /// components with no named fields (unit structs, tuple structs).
    pub fields: &'static [ComponentFieldDescriptor],
    /// Registers the component into a world.
    pub register: fn(&mut World),
}

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
    /// `array:<inner>`, `struct:<path>` — that the C# codegen maps to
    /// a concrete C# type. See `pill_host/src/csharp/codegen.rs`.
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

/// A plain value type (not a component) declared with `#[derive(PillMirror)]`.
///
/// Submitted into the same per-artifact inventory as [`PillComponentDescriptor`]
/// so the host can resolve `struct:<path>` field tags to typed C# structs when
/// it generates mirrors. `Copy` so the module ABI can hand the array to the
/// host by value; the inner `fields` slice stays a pointer into the declaring
/// artifact's static data, which remains mapped for the artifact's lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PillValueTypeDescriptor {
    /// Fully-qualified type name, as `std::any::type_name` would report it.
    pub type_name: &'static str,
    /// Byte size of the value type (`size_of::<T>()`).
    pub size: usize,
    /// Byte alignment of the value type (`align_of::<T>()`).
    pub align: usize,
    /// Field layout of the value type.
    pub fields: &'static [ComponentFieldDescriptor],
}

/// One mirrored method of a `#[derive(PillMirror)]` value type, submitted by
/// the `#[pill_mirror_impl]` attribute macro on the type's `impl` block.
///
/// The macro generates a `#[no_mangle] extern "C"` trampoline for each marked
/// method (so the C ABI is fixed regardless of the Rust method's calling
/// convention) and records it here. The host resolves the trampoline's exported
/// symbol address, hands the table to the C# runtime, and the mirror codegen
/// emits a typed C# instance method that calls it.
///
/// v1 supports a deliberately narrow contract: a `&self` receiver (read-only;
/// writes cannot propagate through the C# pinned-box call), primitive
/// arguments and return values (`u8..u64`, `i8..i64`, `f32`, `f64`, `bool`,
/// `usize`, `isize`), and a `()` return. Everything else is rejected at
/// compile time by the macro.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PillMethodDescriptor {
    /// Fully-qualified type name the method belongs to, as `std::any::type_name`
    /// would report it (`pill_spline::OmoMO`).
    pub type_name: &'static str,
    /// Rust method name, snake_case (`get_sum`); the codegen maps it to
    /// PascalCase for the C# instance method.
    pub name: &'static str,
    /// Exported `#[no_mangle]` symbol of the generated C-ABI trampoline
    /// (`pill_mirror_OmoMO_get_sum`), which the host resolves at load time.
    pub symbol: &'static str,
    /// Type tag of the method's return value from the same closed vocabulary
    /// the field codegen uses (`u64`, `f32`, ...); empty for a `()` return.
    pub return_tag: &'static str,
    /// Type tags of the method's arguments, in declaration order.
    pub arg_tags: &'static [&'static str],
    /// Argument names from the Rust source, in declaration order (`alpha`,
    /// `beta`), so the generated C# mirror names its parameters identically
    /// instead of inventing `arg0`, `arg1`. Parallel to `arg_tags`; the macro
    /// always emits one name per tag (a positional fallback when the pattern
    /// is not a plain identifier).
    pub arg_names: &'static [&'static str],
}

inventory::collect!(PillComponentDescriptor);
inventory::collect!(PillValueTypeDescriptor);
inventory::collect!(PillMethodDescriptor);

/// Every value type this artifact declares with `#[derive(PillMirror)]`,
/// sorted by type name so the host consumes them deterministically.
pub fn value_type_descriptors() -> Vec<&'static PillValueTypeDescriptor> {
    let mut descriptors: Vec<&'static PillValueTypeDescriptor> =
        inventory::iter::<PillValueTypeDescriptor>().collect();
    descriptors.sort_by_key(|descriptor| descriptor.type_name);
    descriptors
}

/// Every mirrored method this artifact declares with `#[pill_mirror_impl]` /
/// `#[pill_mirror_method]`, sorted by type name then method name so the host
/// consumes them deterministically.
pub fn mirror_method_descriptors() -> Vec<&'static PillMethodDescriptor> {
    let mut descriptors: Vec<&'static PillMethodDescriptor> =
        inventory::iter::<PillMethodDescriptor>().collect();
    descriptors.sort_by_key(|descriptor| (descriptor.type_name, descriptor.name));
    descriptors
}

/// Register every component this artifact declares with the derive.
///
/// Called by the macro-generated `init` entry point before the user's own
/// registration code runs, so entity seeding and system registration can rely
/// on every component type already being known to the world.
///
/// # Errors
///
/// Returns the first registration failure recorded during the loop (currently
/// only the 128-type ceiling) so the generated `init` can fail the reload
/// transactionally instead of running with a half-registered component set.
pub fn register_all_components(world: &mut World) -> Result<(), WorldError> {
    for descriptor in inventory::iter::<PillComponentDescriptor> {
        (descriptor.register)(world);
    }
    world.take_registration_error().map_or(Ok(()), Err)
}

/// Aggregate schema fingerprint of every persistable component.
///
/// Deterministic across builds: descriptors are sorted by type name before
/// hashing because the iterator makes no ordering guarantee. The fingerprint is
/// exported by the project-module ABI; keeping it derived from the same
/// registry that drives registration means a new persistable component can
/// never be forgotten from the fingerprint.
pub fn persistable_schema_fingerprint() -> u64 {
    let mut descriptors: Vec<&PillComponentDescriptor> = inventory::iter::<PillComponentDescriptor>
        .into_iter()
        .collect();
    descriptors.sort_by_key(|descriptor| descriptor.type_name);

    let mut hasher = DefaultHasher::new();
    for descriptor in descriptors {
        if descriptor.persistable {
            descriptor.type_name.hash(&mut hasher);
        }
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Component, ComponentId, World};

    // The derive macro cannot be used inside `pill_engine` itself (its
    // generated code refers to `::pill_engine`, which does not resolve in the
    // defining crate), so these components declare everything the derive would
    // generate, by hand, and submit descriptors exactly as the macro does.

    #[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
    struct TestPersistableComponent {
        value: u32,
    }
    impl Component for TestPersistableComponent {}
    trait_type_map::impl_trait_accessible!(dyn Component; TestPersistableComponent);

    fn register_test_persistable(world: &mut World) {
        world.register_persistable_component::<TestPersistableComponent>();
    }

    #[derive(Clone, Debug)]
    struct TestPlainComponent;
    impl Component for TestPlainComponent {}
    trait_type_map::impl_trait_accessible!(dyn Component; TestPlainComponent);

    fn register_test_plain(world: &mut World) {
        world.register_component::<TestPlainComponent>();
    }

    inventory::submit! {
        PillComponentDescriptor {
            type_name: "pill_engine::component_registry::tests::TestPersistableComponent",
            persistable: true,
            fields: &[],
            register: register_test_persistable,
        }
    }
    inventory::submit! {
        PillComponentDescriptor {
            type_name: "pill_engine::component_registry::tests::TestPlainComponent",
            persistable: false,
            fields: &[],
            register: register_test_plain,
        }
    }

    /// The registration loop registers every submitted component exactly once.
    #[test]
    fn registry_registers_every_submitted_component() {
        let mut world = World::new();
        register_all_components(&mut world).expect("registration must succeed");

        assert!(world
            .component_registry
            .get_bit(&ComponentId::of::<TestPersistableComponent>())
            .is_some());
        assert!(world
            .component_registry
            .get_bit(&ComponentId::of::<TestPlainComponent>())
            .is_some());
    }

    /// Registration is idempotent, so running the loop twice is safe.
    #[test]
    fn registry_registration_is_idempotent() {
        let mut world = World::new();
        register_all_components(&mut world).expect("registration must succeed");
        register_all_components(&mut world).expect("re-registration must succeed");

        assert!(world
            .component_registry
            .get_bit(&ComponentId::of::<TestPersistableComponent>())
            .is_some());
    }

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

    /// Mirrored-method descriptors submitted through the inventory are
    /// queryable, sorted by type name then method name.
    #[test]
    fn mirror_method_descriptors_are_collected_and_sorted() {
        crate::submit! {
            PillMethodDescriptor {
                type_name: "pill_engine::test::Zulu",
                name: "alpha",
                symbol: "pill_mirror_Zulu_alpha",
                return_tag: "u64",
                arg_tags: &[],
                arg_names: &[],
            }
        }
        crate::submit! {
            PillMethodDescriptor {
                type_name: "pill_engine::test::Alpha",
                name: "beta",
                symbol: "pill_mirror_Alpha_beta",
                return_tag: "u32",
                arg_tags: &["f32", "u8"],
                arg_names: &["blend", "count"],
            }
        }

        let methods = mirror_method_descriptors();
        let mut matching: Vec<(&'static str, &'static str)> = methods
            .iter()
            .filter(|descriptor| descriptor.type_name.starts_with("pill_engine::test::"))
            .map(|descriptor| (descriptor.type_name, descriptor.name))
            .collect();
        matching.dedup_by(|left, right| left == right);
        assert_eq!(
            matching,
            vec![
                ("pill_engine::test::Alpha", "beta"),
                ("pill_engine::test::Zulu", "alpha"),
            ]
        );

        let beta = methods
            .iter()
            .find(|descriptor| descriptor.name == "beta")
            .expect("the submitted beta descriptor must be present");
        assert_eq!(beta.return_tag, "u32");
        assert_eq!(beta.arg_tags, &["f32", "u8"]);
        assert_eq!(beta.arg_names, &["blend", "count"]);
        assert_eq!(beta.symbol, "pill_mirror_Alpha_beta");
    }

    /// The persistable-only fingerprint is stable across runs.
    #[test]
    fn persistable_fingerprint_is_deterministic() {
        let first = persistable_schema_fingerprint();
        let second = persistable_schema_fingerprint();
        assert_eq!(first, second);
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
