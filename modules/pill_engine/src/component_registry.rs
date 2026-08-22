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
    /// Registers the component into a world.
    pub register: fn(&mut World),
}

inventory::collect!(PillComponentDescriptor);

/// Register every component this artifact declares with the derive.
///
/// Called by the macro-generated `init` entry point before the user's own
/// registration code runs, so entity seeding and system registration can rely
/// on every component type already being known to the world.
pub fn register_all_components(world: &mut World) {
    for descriptor in inventory::iter::<PillComponentDescriptor> {
        (descriptor.register)(world);
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
            register: register_test_persistable,
        }
    }
    inventory::submit! {
        PillComponentDescriptor {
            type_name: "pill_engine::component_registry::tests::TestPlainComponent",
            persistable: false,
            register: register_test_plain,
        }
    }

    /// The registration loop registers every submitted component exactly once.
    #[test]
    fn registry_registers_every_submitted_component() {
        let mut world = World::new();
        register_all_components(&mut world);

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
        register_all_components(&mut world);
        register_all_components(&mut world);

        assert!(world
            .component_registry
            .get_bit(&ComponentId::of::<TestPersistableComponent>())
            .is_some());
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
