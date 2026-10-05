//! Per-DLL component, value-type, method, accessor and export registries.
//!
//! # Responsibilities
//!
//! - Re-export the engine core's field layouts
//!   (`pill_engine_core::component_registry`) under this path.
//! - Declare the descriptor types the derive and attribute macros submit into
//!   [`inventory`] collections, and the collections themselves.
//! - Provide the artifact-wide registration loop, the aggregate schema
//!   fingerprint, and the readers the module/project entry-point macros and the
//!   host call.
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
//! It is per artifact because this crate is: `pill_engine` is embedded in every
//! DLL, while the engine core (`pill_engine_core`) holds what is the same for
//! all of them. The core never reads these collections; what it needs from one
//! (a value type's layout) it receives from the calling DLL.
//!
//! Registration order is unspecified (linker/initializer order), which is
//! harmless: component registration is keyed by `TypeId` and idempotent.

// Standard library
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

// Current crate
use crate::error::WorldError;
use crate::World;

/// Field layouts, the layout trait and the value-type resolver, which are the
/// same for every DLL.
pub use pill_engine_core::component_registry::*;

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

/// One heap-owning field accessor declared by `#[derive(PillComponent)]`,
/// submitted into the same per-artifact inventory as the other descriptors.
///
/// Unlike [`ComponentFieldDescriptor`], which says where a field sits in the
/// row, an accessor says how the field's *buffer* is reached: the derive emits
/// one `#[no_mangle]` trampoline per supported operation and records its
/// symbol here. The host resolves those symbols at load time and hands their
/// addresses to the C# runtime under the operation names `<field>_view`,
/// `<field>_resize`, `<field>_set`, `<field>_item`, `<field>_set_item` and
/// `<field>_push`, so a generated mirror reaches the live buffer instead of
/// copying its header out of the row.
///
/// Every trampoline receives the address of the live component value (the row
/// a managed query is iterating) and works in place: no serialization, no
/// buffer copy, and no allocation on the read path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PillFieldAccessorDescriptor {
    /// Fully-qualified component type name the field belongs to.
    pub type_name: &'static str,
    /// Rust field name (snake_case); the C# codegen derives member names from
    /// it.
    pub field_name: &'static str,
    /// Container kind: `"vec"`, `"dynbuf"`, `"string"`, or `"vecstring"`.
    ///
    /// `"vecstring"` describes a `Vec<String>`: its elements are separately
    /// allocated strings, so no span exists over them and managed code reaches
    /// each element through its own accessor instead.
    pub kind: &'static str,
    /// Element type tag of a `vec` field (`f32`, `struct:<path>`, ...);
    /// `string` for a `vecstring` field; empty for a `string` field.
    pub element_tag: &'static str,
    /// Exported symbol of the view trampoline
    /// (`pill_accessor_{Type}_{field}_view`); empty for a `vecstring` field,
    /// whose elements cannot be viewed as one run.
    pub view_symbol: &'static str,
    /// Exported symbol of the resize trampoline for a `vec`, `dynbuf` or
    /// `vecstring` field; empty for a `string` field.
    pub resize_symbol: &'static str,
    /// Exported symbol of the mutate-in-place trampoline for a `string` field;
    /// empty for every other kind.
    pub set_symbol: &'static str,
    /// Exported symbol of the per-element view trampoline for a `vecstring`
    /// field (`pill_accessor_{Type}_{field}_item`); empty for every other
    /// kind.
    pub item_symbol: &'static str,
    /// Exported symbol of the per-element replace trampoline for a `vecstring`
    /// field; empty for every other kind.
    pub set_item_symbol: &'static str,
    /// Exported symbol of the append trampoline for a `vecstring` field; empty
    /// for every other kind.
    pub push_symbol: &'static str,
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

/// A named C-ABI function an artifact offers to the host, found by name.
///
/// Submitted with `pill_engine::submit!` beside the function, so a host that
/// links the artifact statically - a shipping build, or a host that links a
/// data crate directly - finds it through [`find_export`] with no symbol table
/// to search. A loaded module offers the same function as a `#[no_mangle]`
/// export under the same name, which the host resolves from the DLL instead.
///
/// Keep the shape minimal (name and address): every such function carries its
/// own signature, which the host states where it calls it.
#[derive(Clone, Copy, Debug)]
pub struct PillExportDescriptor {
    /// The function's export name, e.g. `pill_render_data_load_mesh_obj`.
    pub name: &'static str,
    /// The function's address.
    pub address: ExportAddress,
}

/// The address of an exported function, as data.
///
/// A newtype so descriptors can live in a `static` inventory: a raw function
/// pointer is neither `Send` nor `Sync` on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExportAddress(pub *const ());

// SAFETY: the address names immutable code in the image that submitted it; it
// is only ever read and turned back into the function pointer it was made from.
unsafe impl Send for ExportAddress {}
// SAFETY: as above - nothing is written through it.
unsafe impl Sync for ExportAddress {}

inventory::collect!(PillComponentDescriptor);
inventory::collect!(PillValueTypeDescriptor);
inventory::collect!(PillMethodDescriptor);
inventory::collect!(PillExportDescriptor);

/// The address of the export named `name` among those linked into the calling
/// artifact, if one was submitted.
///
/// Sees only this artifact's own inventory: a function in a separately loaded
/// module is not found here, and is resolved from that module's DLL instead.
pub fn find_export(name: &str) -> Option<ExportAddress> {
    inventory::iter::<PillExportDescriptor>
        .into_iter()
        .find(|descriptor| descriptor.name == name)
        .map(|descriptor| descriptor.address)
}
inventory::collect!(PillFieldAccessorDescriptor);

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

/// Every heap-field accessor this artifact declares with
/// `#[derive(PillComponent)]`, sorted by component type name then field name
/// so the host consumes them deterministically.
///
/// The returned descriptors live in this artifact's static data, so their
/// symbol strings stay valid for as long as the artifact is mapped.
pub fn field_accessor_descriptors() -> Vec<&'static PillFieldAccessorDescriptor> {
    let mut descriptors: Vec<&'static PillFieldAccessorDescriptor> =
        inventory::iter::<PillFieldAccessorDescriptor>().collect();
    descriptors.sort_by_key(|descriptor| (descriptor.type_name, descriptor.field_name));
    descriptors
}

/// Register every component this artifact declares with the derive.
///
/// Called by the macro-generated `init` entry point before the user's own
/// registration code runs, so entity seeding and system registration can rely
/// on every component type already being known to the world.
///
/// While the loop runs, this artifact's value types are installed as the
/// world's [`ValueTypeLayoutResolver`], so a component field of a
/// `#[derive(PillMirror)]` type expands into editable rows. The previous
/// resolver is restored afterwards, so none is left pointing into this DLL.
///
/// # Errors
///
/// Returns the first registration failure recorded during the loop (currently
/// only the 128-type ceiling) so the generated `init` can fail the reload
/// transactionally instead of running with a half-registered component set.
pub fn register_all_components(world: &mut World) -> Result<(), WorldError> {
    let previous = world.replace_value_type_layout_resolver(Some(resolve_value_type_layout));
    for descriptor in inventory::iter::<PillComponentDescriptor> {
        (descriptor.register)(world);
    }
    world.replace_value_type_layout_resolver(previous);
    world.take_registration_error().map_or(Ok(()), Err)
}

/// The layout of the value type a `struct:<path>` field tag names, among the
/// ones this artifact declares with `#[derive(PillMirror)]`.
///
/// The tag is the path as written at the field site, usually an imported
/// short name (`Color`), so an exact type name is tried first and a unique
/// `::<path>` suffix second. An ambiguous suffix resolves to `None`: an opaque
/// field is readable, a wrong one is not.
pub fn resolve_value_type_layout(path: &str) -> Option<&'static [ComponentFieldDescriptor]> {
    let suffix = format!("::{path}");
    let mut suffix_hit = None;
    let mut suffix_matches = 0usize;
    for descriptor in value_type_descriptors() {
        if descriptor.type_name == path {
            return Some(descriptor.fields);
        }
        if descriptor.type_name.ends_with(&suffix) {
            suffix_matches += 1;
            suffix_hit = Some(descriptor.fields);
        }
    }
    if suffix_matches == 1 {
        suffix_hit
    } else {
        None
    }
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

    // These components declare by hand everything the derive would generate,
    // and submit descriptors exactly as the macro does.

    #[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
    struct TestPersistableComponent {
        value: u32,
    }
    impl Component for TestPersistableComponent {}

    fn register_test_persistable(world: &mut World) {
        world.register_persistable_component::<TestPersistableComponent>();
    }

    #[derive(Clone, Debug)]
    struct TestPlainComponent;
    impl Component for TestPlainComponent {}

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
            .component_registry()
            .get_bit(&ComponentId::of::<TestPersistableComponent>())
            .is_some());
        assert!(world
            .component_registry()
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
            .component_registry()
            .get_bit(&ComponentId::of::<TestPersistableComponent>())
            .is_some());
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
}
