//! Component persistence and schema migration for hot-reload.
//!
//! # Responsibilities
//!
//! - Snapshots all entity component data before a hot-reload using serde_json.
//! - Restores data after new component types are registered, matching old→new
//!   components by type name (not TypeId), so renamed/reshaped components are
//!   handled gracefully.
//! - Stores per-component-type serialize/deserialize/insert function pointers
//!   registered alongside each persistable component.
//!
//! # Design
//!
//! Uses **serde_json** (not bincode) because JSON is self-describing:
//! field names are embedded in the payload, so adding/removing fields does
//! not break deserialization.  New fields receive `Default::default()`,
//! removed fields are silently ignored.
//!
//! Each persistable component type registers three monomorphized functions:
//!
//! | Function        | Signature | Purpose |
//! |-----------------|-----------|---------|
//! | `serialize`     | `fn(&ComponentColumns, index) → Vec<u8>` | Read concrete component from archetype column, JSON-encode it |
//! | `deserialize`   | `fn(&[u8]) → Option<Box<dyn Component>>` | Decode JSON bytes back into a component; returns None on schema mismatch |
//! | `insert_boxed`  | `fn(&mut ComponentColumns, Box<dyn Component>)` | Downcast and push into the archetype's column |
//!
//! These functions are monomorphized in the project DLL (where the concrete
//! types are defined).  They are stored as plain function pointers in the
//! engine's `World`, so replacing them on reload (via `HashMap::insert`)
//! does not call any destructors through old vtables — function pointers
//! are trivially overwritten.
//!
//! During snapshot: iterate every archetype, call `serialize` for each
//! (entity, component_type) pair.
//!
//! During restore: destroy all existing entities, then for each snapshot
//! entry, `deserialize` → `Option<Box<dyn Component>>`, call `insert_boxed`
//! into the target archetype's storage.

// Standard library
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

// External crates
use pill_core::{debug, error, info, warn};
use serde::{de::DeserializeOwned, Serialize};

// Current crate
use crate::archetype::ComponentColumns;
use crate::component::{Component, ComponentId};
use crate::entity::Entity;
use crate::error::{
    PersistenceError, WorldError, COMPONENT_LAYOUT_CHANGED_REFUSAL,
    COMPONENT_NAME_COLLISION_REFUSAL,
};
use crate::resource::{ErasedResource, Resource, ResourceId};
use crate::world::World;

// =============================================================================
// Module Tree
// =============================================================================

mod components;
mod migration;
mod registry;
mod resources;
mod snapshot;

pub use components::*;
pub use registry::*;
pub use resources::*;

// =============================================================================
// World — Additional Fields
// =============================================================================
//
// These fields are added to the `World` struct (see `world.rs`):
//
// ```ignore
// /// Per-component-type serialize fn for snapshotting.
// pub(crate) persist_serializers: HashMap<ComponentId, SerializeComponentFn>,
// /// Per-type-name deserialize fn for restoring.
// pub(crate) persist_deserializers: HashMap<String, DeserializeComponentFn>,
// /// Per-component-type insert fn for pushing Box<dyn Component> into storage.
// pub(crate) persist_inserters: HashMap<ComponentId, InsertComponentFn>,
// ```

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archetype::Blittability;

    #[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
    struct DropTestForgottenComponent {
        value: u32,
    }
    impl Component for DropTestForgottenComponent {}

    #[derive(Clone, Debug)]
    struct DropTestKeptComponent {
        value: u32,
    }
    impl Component for DropTestKeptComponent {}

    /// A distinct type that *declares* another type's name, standing in for the
    /// registration a rebuilt image makes: same name, fresh `TypeId`.
    #[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
    struct DropTestSupersedingComponent {
        value: u32,
    }
    impl Component for DropTestSupersedingComponent {
        fn shared_name() -> Option<&'static str> {
            Some(std::any::type_name::<DropTestForgottenComponent>())
        }
    }

    /// A persistable component whose column is 8 bytes / align 4.
    #[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
    struct LayoutHostComponent {
        a: u32,
        b: u32,
    }
    impl Component for LayoutHostComponent {}

    /// Declares the host's name while widening `b` to `f64` - the f32-to-f64
    /// shape that registers 16 bytes / align 8 over the host's column.
    #[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
    struct LayoutWidenedComponent {
        a: u32,
        b: f64,
    }
    impl Component for LayoutWidenedComponent {
        fn shared_name() -> Option<&'static str> {
            Some(std::any::type_name::<LayoutHostComponent>())
        }
    }

    /// Same name, same alignment, one field more: the add-a-field shape that
    /// must keep registering (and migrating).
    #[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
    struct LayoutGrownComponent {
        a: u32,
        b: u32,
        c: u32,
    }
    impl Component for LayoutGrownComponent {
        fn shared_name() -> Option<&'static str> {
            Some(std::any::type_name::<LayoutHostComponent>())
        }
    }

    /// The refusal sentence is printed verbatim into host logs and grepped by
    /// humans, so it carries no source-wrap artifacts.
    #[test]
    fn collision_refusal_message_has_no_wrap_spaces() {
        assert!(
            !COMPONENT_NAME_COLLISION_REFUSAL.contains("  "),
            "{COMPONENT_NAME_COLLISION_REFUSAL}"
        );
        assert!(!COMPONENT_NAME_COLLISION_REFUSAL.contains('\n'));
        assert!(COMPONENT_NAME_COLLISION_REFUSAL.ends_with("persist entries"));
    }

    /// A superseding registration whose layout the old column cannot host is
    /// refused before anything registers: the host rolls the reload back and
    /// the running generation keeps its storage, instead of the migration
    /// reading the new alignment out of the old column's slots.
    #[test]
    fn a_layout_the_old_column_cannot_host_is_refused() {
        let mut world = World::new();
        world.register_persistable_component::<LayoutHostComponent>();
        let host_id = ComponentId::of::<LayoutHostComponent>();
        let type_name = std::any::type_name::<LayoutHostComponent>().to_string();
        let entity = world
            .create_entity()
            .with(LayoutHostComponent { a: 1, b: 2 })
            .build()
            .unwrap();
        let _ = world.take_registration_error();

        // The rebuilt image's registration: same name, wider alignment.
        world.supersede_persist_registrations(std::slice::from_ref(&type_name));
        world.register_persistable_component::<LayoutWidenedComponent>();

        let error = world
            .take_registration_error()
            .expect("a layout the old column cannot host must be refused");
        let message = error.to_string();
        assert!(
            message.contains("was re-registered with a different size"),
            "the refusal keeps the sentence the migration suite greps for: {message}"
        );
        match error {
            WorldError::ComponentLayoutChanged {
                existing_size,
                existing_align,
                incoming_size,
                incoming_align,
                ..
            } => {
                assert_eq!((existing_size, existing_align), (8, 4));
                assert_eq!((incoming_size, incoming_align), (16, 8));
            }
            other => panic!("expected a ComponentLayoutChanged refusal, got {other:?}"),
        }

        // Nothing registered, and the predecessor kept its registration.
        assert_eq!(
            world.component_registry().registered_components().count(),
            1,
            "a refused registration leaves the registry as it was"
        );
        assert_eq!(world.live_row_count(host_id), 1);
        assert!(world.entity_locations.contains_key(&entity));
        assert!(!world
            .storage_factories
            .contains_key(&ComponentId::of::<LayoutWidenedComponent>()));
    }

    /// A size change that keeps the alignment registers: the old column's
    /// slots stay validly aligned for the incoming type, which is what the
    /// add-a-field reload scenarios depend on.
    #[test]
    fn a_size_change_that_keeps_the_alignment_registers() {
        let mut world = World::new();
        world.register_persistable_component::<LayoutHostComponent>();
        let type_name = std::any::type_name::<LayoutHostComponent>().to_string();
        let _ = world.take_registration_error();

        world.supersede_persist_registrations(std::slice::from_ref(&type_name));
        world.register_persistable_component::<LayoutGrownComponent>();

        assert!(
            world.take_registration_error().is_none(),
            "a wider field of the same alignment must register"
        );
        let grown_layout = world
            .component_registry()
            .get_layout(&ComponentId::of::<LayoutGrownComponent>())
            .expect("the successor is registered");
        assert_eq!((grown_layout.size, grown_layout.align), (12, 4));
    }

    /// Dropping a forgotten type removes its columns from every entity while
    /// entities that carry other components survive with those intact.
    #[test]
    fn drop_forgotten_components_removes_only_the_forgotten_data() {
        let mut world = World::new();
        world.register_persistable_component::<DropTestForgottenComponent>();
        world.register_component::<DropTestKeptComponent>();

        let forgotten_id = ComponentId::of::<DropTestForgottenComponent>();
        let kept_id = ComponentId::of::<DropTestKeptComponent>();

        let entity_only_forgotten = world
            .create_entity()
            .with(DropTestForgottenComponent { value: 1 })
            .build()
            .unwrap();
        let entity_with_both = world
            .create_entity()
            .with(DropTestForgottenComponent { value: 2 })
            .with(DropTestKeptComponent { value: 3 })
            .build()
            .unwrap();

        // The data is readable before the drop.
        assert_eq!(
            world
                .get_component::<DropTestForgottenComponent>(entity_with_both)
                .unwrap()
                .value,
            2
        );

        let type_name = std::any::type_name::<DropTestForgottenComponent>().to_string();
        let dropped = world.drop_forgotten_components(std::slice::from_ref(&type_name));
        assert_eq!(dropped, 2, "both entities carried the forgotten component");

        // The entity that only carried the forgotten component is destroyed;
        // the entity that also carries a kept component survives.
        assert!(!world.entity_locations.contains_key(&entity_only_forgotten));
        assert!(world.entity_locations.contains_key(&entity_with_both));
        assert_eq!(world.entity_locations.len(), 1);

        // The surviving entity's archetype keeps the kept component and no
        // longer contains the forgotten one.
        let location = world.entity_locations[&entity_with_both];
        let archetype = &world.archetypes[&location.archetype_id];
        assert!(archetype.component_types.contains(&kept_id));
        assert!(!archetype.component_types.contains(&forgotten_id));

        // The kept component's value survives untouched.
        let kept = world
            .get_component::<DropTestKeptComponent>(entity_with_both)
            .expect("kept component should still be readable");
        assert_eq!(kept.value, 3);

        // The persistable manifest no longer knows the forgotten type.
        assert!(world
            .persist_type_manifest()
            .iter()
            .all(|entry| entry.type_name != type_name));

        // Re-registering the forgotten type works and fresh data can be seeded.
        world.register_persistable_component::<DropTestForgottenComponent>();
        let reseeded = world
            .create_entity()
            .with(DropTestForgottenComponent { value: 9 })
            .build()
            .unwrap();
        assert!(world.entity_locations.contains_key(&reseeded));
    }

    /// Every native id sharing the forgotten name loses its rows, not just the
    /// one a name resolver returns: an ambiguous name resolved to nothing at
    /// all, so the purge used to skip both generations while
    /// `forget_component_type` removed their registrations anyway.
    #[test]
    fn forget_strips_rows_for_every_id_sharing_the_name() {
        let mut world = World::new();
        world.register_persistable_component::<DropTestForgottenComponent>();
        let first_id = ComponentId::of::<DropTestForgottenComponent>();
        let type_name = std::any::type_name::<DropTestForgottenComponent>().to_string();

        let first_entity = world
            .create_entity()
            .with(DropTestForgottenComponent { value: 1 })
            .build()
            .unwrap();
        assert_eq!(world.live_row_count(first_id), 1);

        // A rebuilt image's registration: same name, fresh id, its own rows.
        world.supersede_persist_registrations(std::slice::from_ref(&type_name));
        world.register_persistable_component::<DropTestSupersedingComponent>();
        let second_id = ComponentId::of::<DropTestSupersedingComponent>();
        assert_ne!(second_id, first_id);
        let second_entity = world
            .create_entity()
            .with(DropTestSupersedingComponent { value: 2 })
            .build()
            .unwrap();
        assert_eq!(world.live_row_count(second_id), 1);

        let dropped = world.drop_forgotten_components(std::slice::from_ref(&type_name));
        assert_eq!(
            dropped, 2,
            "both generations' rows are stripped, not just one id's"
        );
        assert!(!world.entity_locations.contains_key(&first_entity));
        assert!(!world.entity_locations.contains_key(&second_entity));
        assert!(!world.storage_factories.contains_key(&first_id));
        assert!(!world.storage_factories.contains_key(&second_id));
        assert!(!world.component_copiers.contains_key(&first_id));
        assert!(!world.component_copiers.contains_key(&second_id));
    }

    /// A refused persistable registration changes nothing: the guard runs
    /// before the registration does, so no bit, name entry, storage factory or
    /// log entry is created for a call that returns without registering.
    #[test]
    fn a_refused_persistable_registration_changes_nothing() {
        let mut world = World::new();
        world.register_persistable_component::<DropTestForgottenComponent>();
        // A peer with live rows under the same name: without the supersede
        // announcement the guard refuses instead of evicting it.
        world.register_persistable_component::<DropTestSupersedingComponent>();
        let peer_id = ComponentId::of::<DropTestSupersedingComponent>();
        let peer = world
            .create_entity()
            .with(DropTestSupersedingComponent { value: 1 })
            .build()
            .unwrap();
        assert_eq!(world.live_row_count(peer_id), 1);
        let _ = world.take_registration_error();

        let registry_count = world.component_registry().registered_components().count();
        for attempt in 0..3 {
            world.register_persistable_component::<DropTestForgottenComponent>();
            let error = world
                .take_registration_error()
                .unwrap_or_else(|| panic!("attempt {attempt} must be refused"));
            assert!(matches!(error, WorldError::ComponentNameCollision { .. }));
            assert_eq!(
                world.component_registry().registered_components().count(),
                registry_count,
                "a refused attempt leaves the registry as it was"
            );
        }
        assert!(world.entity_locations.contains_key(&peer));
        assert_eq!(
            world.live_row_count(peer_id),
            1,
            "the peer keeps its rows across every refused attempt"
        );
    }

    /// The id-keyed purge removes the rows and every registration artifact of
    /// the ids it is given, while a same-name re-registration - the rollback
    /// generation, with a fresh id - keeps the name-keyed entries alive.
    #[test]
    fn stranded_ids_lose_their_rows_and_factories() {
        let mut world = World::new();
        world.register_persistable_component::<DropTestForgottenComponent>();
        let stranded_id = ComponentId::of::<DropTestForgottenComponent>();
        let type_name = std::any::type_name::<DropTestForgottenComponent>().to_string();

        let entity = world
            .create_entity()
            .with(DropTestForgottenComponent { value: 4 })
            .build()
            .unwrap();
        assert_eq!(world.live_row_count(stranded_id), 1);

        // The rollback generation re-registers the same name under a fresh id,
        // exactly as a rebuilt image does.
        world.supersede_persist_registrations(std::slice::from_ref(&type_name));
        world.register_persistable_component::<DropTestSupersedingComponent>();
        let successor_id = ComponentId::of::<DropTestSupersedingComponent>();
        assert_ne!(successor_id, stranded_id);

        let dropped = world.drop_forgotten_component_ids(&[stranded_id]);
        assert_eq!(dropped, 1, "the stranded type's row was removed with it");

        // The failed generation's id holds nothing anywhere...
        assert!(!world.entity_locations.contains_key(&entity));
        assert!(!world.storage_factories.contains_key(&stranded_id));
        assert!(!world.component_copiers.contains_key(&stranded_id));
        assert!(!world.persist_serializers.contains_key(&stranded_id));
        assert!(!world.persist_inserters.contains_key(&stranded_id));

        // ...while the rollback generation's own entries survive, including
        // the name-keyed ones the two registrations share.
        assert!(world.persist_serializers.contains_key(&successor_id));
        assert!(world.persist_inserters.contains_key(&successor_id));
        assert!(world.persist_deserializers.contains_key(&type_name));
        assert!(world.persist_schema_hashes.contains_key(&type_name));
    }

    /// The registration log distinguishes a type that was dropped entirely
    /// from one merely downgraded to a plain component.
    #[test]
    fn registered_component_names_since_covers_plain_components() {
        let mut world = World::new();
        let sequence = world.component_registration_sequence();

        world.register_persistable_component::<DropTestForgottenComponent>();
        world.register_component::<DropTestKeptComponent>();

        let registered = world.registered_component_names_since(sequence);
        assert!(registered
            .iter()
            .any(|name| name.contains("DropTestForgottenComponent")));
        assert!(registered
            .iter()
            .any(|name| name.contains("DropTestKeptComponent")));

        let persistable = world.persist_type_names_registered_since(sequence);
        assert!(persistable
            .iter()
            .any(|name| name.contains("DropTestForgottenComponent")));
        assert!(!persistable
            .iter()
            .any(|name| name.contains("DropTestKeptComponent")));
    }

    /// Re-registering the same persistable type is idempotent across the
    /// persist maps: one serializer/inserter per ComponentId and one
    /// deserializer/schema hash per name. Pins the invariant the unified
    /// registration struct (audit 4.2) must preserve.
    #[test]
    fn persistable_registration_is_idempotent_across_repeated_calls() {
        let mut world = World::new();
        let component_id = ComponentId::of::<DropTestForgottenComponent>();
        world.register_persistable_component::<DropTestForgottenComponent>();
        world.register_persistable_component::<DropTestForgottenComponent>();

        assert_eq!(world.persist_serializers.len(), 1);
        assert!(world.persist_serializers.contains_key(&component_id));
        assert_eq!(world.persist_inserters.len(), 1);
        assert!(world.persist_inserters.contains_key(&component_id));

        let type_name = std::any::type_name::<DropTestForgottenComponent>().to_string();
        assert_eq!(world.persist_deserializers.len(), 1);
        assert!(world.persist_deserializers.contains_key(&type_name));
        assert_eq!(world.persist_schema_hashes.len(), 1);
        assert!(world.persist_schema_hashes.contains_key(&type_name));
    }

    // =========================================================================
    // Concurrent-peer guard
    // =========================================================================

    /// A second registration of a type name whose existing column still holds
    /// rows is a concurrent peer, not a superseded generation, so it is
    /// reported instead of evicting the peer's persist entries and silently
    /// dropping its rows at the next reload.
    #[test]
    fn a_same_name_registration_over_live_rows_is_reported_not_evicted() {
        let mut world = World::new();
        world.register_persistable_component::<DropTestForgottenComponent>();
        let native_id = ComponentId::of::<DropTestForgottenComponent>();
        let type_name = std::any::type_name::<DropTestForgottenComponent>().to_string();

        // Give the native column a live row, which is what makes the second
        // registration a peer rather than a dead generation.
        world
            .create_entity()
            .with(DropTestForgottenComponent { value: 7 })
            .build()
            .unwrap();
        assert_eq!(world.live_row_count(native_id), 1);

        // A second component claiming the same name arrives - the in-process
        // stand-in for a second binary that linked the same type.
        let error = world
            .register_component_descriptor(
                0x5EED,
                type_name.clone(),
                4,
                4,
                0,
                Blittability::engine_verified(),
            )
            .unwrap_err();

        match error {
            WorldError::ComponentNameCollision {
                type_name: reported_name,
                existing_id,
                live_rows,
                ..
            } => {
                assert_eq!(reported_name, type_name);
                assert_eq!(existing_id, native_id);
                assert_eq!(live_rows, 1);
            }
            other => panic!("expected a name collision, got {other:?}"),
        }

        // The peer's persist entries are untouched, so its rows still restore.
        assert!(world.persist_inserters.contains_key(&native_id));
        assert!(world.persist_serializers.contains_key(&native_id));
    }

    /// The same collision is allowed through once the existing column has no
    /// rows left: that is a superseded generation, and replacing it is the
    /// behaviour hot reload depends on.
    #[test]
    fn a_same_name_registration_over_an_empty_column_still_succeeds() {
        let mut world = World::new();
        world.register_persistable_component::<DropTestForgottenComponent>();
        let type_name = std::any::type_name::<DropTestForgottenComponent>().to_string();

        // No entity is created, so the native column holds nothing.
        assert_eq!(
            world.live_row_count(ComponentId::of::<DropTestForgottenComponent>()),
            0
        );

        let result = world.register_component_descriptor(
            0x5EED,
            type_name,
            4,
            4,
            0,
            Blittability::engine_verified(),
        );
        assert!(
            result.is_ok(),
            "an empty same-name column is a dead generation, not a peer: {result:?}"
        );
    }

    /// The same collision shape as the peer test above - same name, different
    /// id, live rows - but announced by the host as the retiring generation.
    /// That registration is the predecessor and must replace the entries, which
    /// is the case every project reload runs through.
    #[test]
    fn an_announced_name_replaces_its_live_predecessor() {
        let mut world = World::new();
        world.register_persistable_component::<DropTestForgottenComponent>();
        let predecessor_id = ComponentId::of::<DropTestForgottenComponent>();
        let type_name = std::any::type_name::<DropTestForgottenComponent>().to_string();
        world
            .create_entity()
            .with(DropTestForgottenComponent { value: 7 })
            .build()
            .unwrap();
        assert_eq!(world.live_row_count(predecessor_id), 1);

        let successor_id = ComponentId::of::<DropTestSupersedingComponent>();
        world.supersede_persist_registrations(&[type_name]);
        world.register_persistable_component::<DropTestSupersedingComponent>();

        // Replaced, not refused: the successor's entries are the live ones now.
        assert!(world.take_registration_error().is_none());
        assert!(!world.persist_serializers.contains_key(&predecessor_id));
        assert!(world.persist_serializers.contains_key(&successor_id));
        assert!(world.persist_inserters.contains_key(&successor_id));
        // The predecessor's rows are untouched: rehoming them is the reload's
        // step, not the registry's.
        assert_eq!(world.live_row_count(predecessor_id), 1);

        // The announcement was consumed with that one registration. With the
        // successor now holding a row of its own, the same arrival the mark
        // permitted a moment ago is refused again.
        world
            .create_entity()
            .with(DropTestSupersedingComponent { value: 1 })
            .build()
            .unwrap();
        world.register_persistable_component::<DropTestForgottenComponent>();
        assert!(matches!(
            world.take_registration_error(),
            Some(WorldError::ComponentNameCollision { .. })
        ));
    }

    // =========================================================================
    // Retiring drop glue across an in-place migration
    // =========================================================================

    /// Rows dropped by each stand-in's glue, so a migration can be asked
    /// *which* generation's destructor released the old values.
    static RETIRING_GLUE_DROPS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    static ARRIVING_GLUE_DROPS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    /// The retiring generation of a shared persistable type.
    #[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
    struct RetiringGlueComponent {
        // The field carries the shape the serde row and the size check see;
        // the drop accounting never reads it.
        #[allow(dead_code)]
        value: u32,
    }
    impl Component for RetiringGlueComponent {
        fn shared_name() -> Option<&'static str> {
            Some("audit::DropGlue")
        }
    }
    impl Drop for RetiringGlueComponent {
        fn drop(&mut self) {
            RETIRING_GLUE_DROPS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// The arriving generation: same declared name and shape, different glue.
    #[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
    struct ArrivingGlueComponent {
        // Same shape as the retiring copy; nothing reads the value here
        // either.
        #[allow(dead_code)]
        value: u32,
    }
    impl Component for ArrivingGlueComponent {
        fn shared_name() -> Option<&'static str> {
            Some("audit::DropGlue")
        }
    }
    impl Drop for ArrivingGlueComponent {
        fn drop(&mut self) {
            ARRIVING_GLUE_DROPS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// The in-place migration drops old-layout rows through the glue that wrote
    /// them. The sequence that used to break it: the arriving generation's
    /// registration replaces the factory, `rehome_native_columns` stamps the
    /// arriving table onto the still-old column, and only then does the
    /// migration consume that column.
    #[test]
    fn in_place_migration_drops_old_rows_through_the_retiring_glue() {
        use std::sync::atomic::Ordering;

        let mut world = World::new();
        world.register_persistable_component::<RetiringGlueComponent>();
        let component_id = ComponentId::of::<RetiringGlueComponent>();
        let type_name =
            crate::component::ComponentRegistry::registered_name::<RetiringGlueComponent>()
                .to_string();

        let entity = world
            .create_entity()
            .with(RetiringGlueComponent { value: 3 })
            .build()
            .unwrap();
        assert!(world.entity_locations.contains_key(&entity));
        world.rehome_native_columns();

        // Capture the retiring generation's serializer while it is still the
        // registered one, then stand in for the rebuilt image's registration.
        let serialize_old = world.persist_serializers[&component_id];
        world.register_persistable_component::<ArrivingGlueComponent>();
        let serialize_current = world.persist_serializers[&component_id];
        let deserialize_new = world.persist_deserializers[&type_name];
        let insert_new = world.persist_inserters[&component_id];

        // The re-home the reload runs before migration stamps the arriving
        // generation's table onto the old column - the state that produced the
        // invalid free.
        world.rehome_native_columns();

        let retiring_before = RETIRING_GLUE_DROPS.load(Ordering::SeqCst);
        let arriving_before = ARRIVING_GLUE_DROPS.load(Ordering::SeqCst);
        let migrated = world
            .migrate_component_column_in_place(
                component_id,
                serialize_old,
                serialize_current,
                deserialize_new,
                insert_new,
                None,
            )
            .expect("the in-place migration runs on a native column");
        assert_eq!(migrated, 1);

        assert_eq!(
            RETIRING_GLUE_DROPS.load(Ordering::SeqCst),
            retiring_before + 1,
            "the old row is released by the generation that allocated it"
        );
        // `deserialize_component` materializes a `T::default()` schema baseline
        // per row, and that baseline is dropped through the arriving type's
        // glue. One row was migrated, so exactly one arriving drop is the
        // baseline - the old row itself must not be among them.
        assert_eq!(
            ARRIVING_GLUE_DROPS.load(Ordering::SeqCst),
            arriving_before + 1,
            "the arriving glue released only its own schema baseline, never the old row"
        );
    }

    /// Re-registering a persistable type while its own column holds rows is
    /// the ordinary hot-reload path and must not trip the peer guard: the
    /// guard only looks at *other* component ids.
    #[test]
    fn re_registering_the_same_type_over_live_rows_is_not_a_collision() {
        let mut world = World::new();
        world.register_persistable_component::<DropTestForgottenComponent>();
        world
            .create_entity()
            .with(DropTestForgottenComponent { value: 1 })
            .build()
            .unwrap();

        world.register_persistable_component::<DropTestForgottenComponent>();
        assert!(
            world.take_registration_error().is_none(),
            "a reload re-registering its own type is not a peer collision"
        );
    }

    // =========================================================================
    // Name resolution
    // =========================================================================

    /// A name claimed by exactly one registration resolves to it, through both
    /// the persistable-filtered resolver and the unfiltered one.
    #[test]
    fn an_unambiguous_name_resolves_through_both_resolvers() {
        let mut world = World::new();
        world.register_persistable_component::<DropTestForgottenComponent>();
        let component_id = ComponentId::of::<DropTestForgottenComponent>();
        let type_name = std::any::type_name::<DropTestForgottenComponent>();

        assert_eq!(
            world.resolve_component_id_by_name(type_name).unwrap(),
            Some(component_id)
        );
        assert_eq!(
            world.resolve_component_id_by_name_any(type_name).unwrap(),
            Some(component_id)
        );
    }

    /// A name claimed by two registrations is reported as ambiguous rather
    /// than resolved by the old `max_by_key(bit)` tiebreak, which is not a
    /// recency ordering and could bind callers to the wrong column.
    #[test]
    fn a_name_claimed_twice_resolves_to_an_ambiguity_error() {
        let mut world = World::new();
        world.register_persistable_component::<DropTestForgottenComponent>();
        let type_name = std::any::type_name::<DropTestForgottenComponent>().to_string();

        // The native column is empty, so a second claim on the name registers
        // (see the empty-column test above) and both are now visible to the
        // unfiltered resolver.
        world
            .register_component_descriptor(
                0x5EED,
                type_name.clone(),
                4,
                4,
                0,
                Blittability::engine_verified(),
            )
            .unwrap();

        let error = world
            .resolve_component_id_by_name_any(&type_name)
            .unwrap_err();
        match error {
            WorldError::ComponentNameAmbiguous {
                type_name: reported_name,
                count,
            } => {
                assert_eq!(reported_name, type_name);
                assert_eq!(count, 2);
            }
            other => panic!("expected an ambiguity error, got {other:?}"),
        }

        // The persistable-filtered resolver still collapses to one, because
        // only the native registration has an inserter.
        assert_eq!(
            world.resolve_component_id_by_name(&type_name).unwrap(),
            Some(ComponentId::of::<DropTestForgottenComponent>())
        );
    }

    /// A name nothing claims resolves to `None` rather than an error - an
    /// unregistered type is an ordinary outcome, not an ambiguity.
    #[test]
    fn an_unclaimed_name_resolves_to_none() {
        let world = World::new();
        assert_eq!(
            world
                .resolve_component_id_by_name("nothing::Registered")
                .unwrap(),
            None
        );
        assert_eq!(
            world
                .resolve_component_id_by_name_any("nothing::Registered")
                .unwrap(),
            None
        );
    }
    // =========================================================================
    // Descriptor-only persistence (C.3)
    // =========================================================================

    /// Register a descriptor-only component with a two-field layout.
    ///
    /// Mirrors what the C# manifest path registers: a blittable row plus the
    /// field descriptors that describe it, which together are everything the
    /// generic codec needs.
    fn register_probe_descriptor_component(world: &mut World, name: &str) -> ComponentId {
        let component_id = world
            .register_component_descriptor(
                0xC3_0001,
                name.to_string(),
                8,
                4,
                77,
                Blittability::engine_verified(),
            )
            .expect("a valid descriptor layout registers");
        world
            .register_component_descriptor_with_layout(
                component_id,
                vec![
                    crate::component_registry::ComponentFieldDescriptor {
                        name: "health",
                        type_tag: "i32",
                        offset: 0,
                        size: 4,
                        align: 4,
                        element_count: 0,
                    },
                    crate::component_registry::ComponentFieldDescriptor {
                        name: "speed",
                        type_tag: "f32",
                        offset: 4,
                        size: 4,
                        align: 4,
                        element_count: 0,
                    },
                ],
            )
            .expect("the layout fits the registered size");
        component_id
    }

    /// A descriptor-only component survives a snapshot and restore.
    ///
    /// Before the descriptor codec existed this component was absent from the
    /// snapshot entirely: `snapshot_components` consulted `persist_serializers`,
    /// which only a Rust type can populate, so a C#-declared component was
    /// silently dropped by a save.
    #[test]
    fn a_descriptor_only_component_round_trips_through_a_snapshot() {
        let mut world = World::new();
        let component_id = register_probe_descriptor_component(&mut world, "probe::Stats");

        let entity = world.create_entity().build().unwrap();
        world
            .add_descriptor_component(entity, component_id, &[7, 0, 0, 0, 0, 0, 160, 64])
            .expect("the row is the registered width");

        let snapshot = world.snapshot_components();
        assert_eq!(snapshot.entries.len(), 1, "the entity reached the snapshot");
        assert_eq!(
            snapshot.entries[0].len(),
            1,
            "its descriptor component reached the snapshot"
        );
        assert_eq!(snapshot.entries[0][0].0, "probe::Stats");

        world.restore_from_snapshot(&snapshot);

        let restored: Vec<Entity> = world.entity_locations.keys().copied().collect();
        assert_eq!(restored.len(), 1, "one entity comes back");
        let location = world.entity_locations[&restored[0]];
        let archetype = &world.archetypes[&location.archetype_id];
        let row = archetype
            .component_storages
            .get(component_id)
            .expect("the restored archetype owns the column")
            .bytes(location.index_in_archetype)
            .expect("the row exists");
        assert_eq!(
            row,
            &[7, 0, 0, 0, 0, 0, 160, 64],
            "every byte of the row survives the round trip"
        );
    }

    /// A field the snapshot does not carry restores to defined bytes.
    #[test]
    fn a_field_added_since_the_snapshot_restores_zeroed() {
        let fields = [
            crate::component_registry::ComponentFieldDescriptor {
                name: "health",
                type_tag: "i32",
                offset: 0,
                size: 4,
                align: 4,
                element_count: 0,
            },
            crate::component_registry::ComponentFieldDescriptor {
                name: "shield",
                type_tag: "i32",
                offset: 4,
                size: 4,
                align: 4,
                element_count: 0,
            },
        ];

        // A snapshot written before `shield` existed.
        let image = crate::component_field::descriptor_row_image(br#"{"health":9}"#, &fields, 8)
            .expect("the payload is an object");

        assert_eq!(&image[0..4], &9i32.to_le_bytes(), "the carried field lands");
        assert_eq!(
            &image[4..8],
            &[0, 0, 0, 0],
            "the added field starts defined rather than reading stale memory"
        );
    }

    /// A reordered field follows its name, not its position.
    #[test]
    fn a_reordered_field_restores_by_name() {
        let before = [
            crate::component_registry::ComponentFieldDescriptor {
                name: "health",
                type_tag: "i32",
                offset: 0,
                size: 4,
                align: 4,
                element_count: 0,
            },
            crate::component_registry::ComponentFieldDescriptor {
                name: "speed",
                type_tag: "i32",
                offset: 4,
                size: 4,
                align: 4,
                element_count: 0,
            },
        ];
        // The same two fields, swapped.
        let after = [
            crate::component_registry::ComponentFieldDescriptor {
                name: "speed",
                type_tag: "i32",
                offset: 0,
                size: 4,
                align: 4,
                element_count: 0,
            },
            crate::component_registry::ComponentFieldDescriptor {
                name: "health",
                type_tag: "i32",
                offset: 4,
                size: 4,
                align: 4,
                element_count: 0,
            },
        ];

        let mut row = Vec::new();
        row.extend_from_slice(&11i32.to_le_bytes());
        row.extend_from_slice(&22i32.to_le_bytes());
        let json = crate::component_field::serialize_descriptor_row(&row, &before);

        let image = crate::component_field::descriptor_row_image(&json, &after, 8)
            .expect("the payload is an object");
        assert_eq!(
            &image[0..4],
            &22i32.to_le_bytes(),
            "speed moved to offset 0"
        );
        assert_eq!(
            &image[4..8],
            &11i32.to_le_bytes(),
            "health moved to offset 4"
        );
    }

    /// A field the codec cannot interpret still survives the round trip.
    ///
    /// The editor refuses to *write* a `struct:` field; persistence must still
    /// *preserve* it, which is why the two paths do not share a rule.
    #[test]
    fn an_uninterpretable_field_round_trips_as_bytes() {
        let fields = [crate::component_registry::ComponentFieldDescriptor {
            name: "nested",
            type_tag: "struct:probe::Inner",
            offset: 0,
            size: 4,
            align: 4,
            element_count: 0,
        }];

        let row = [1u8, 2, 3, 4];
        let json = crate::component_field::serialize_descriptor_row(&row, &fields);
        let image = crate::component_field::descriptor_row_image(&json, &fields, 4)
            .expect("the payload is an object");
        assert_eq!(&image[..], &row[..], "the opaque bytes come back unchanged");
    }

    // =========================================================================
    // Resource persistence
    // =========================================================================

    /// An ordinary project resource: private identity, serde-able shape.
    #[derive(Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
    struct PersistProbeScore {
        points: u32,
        label: String,
    }
    impl crate::resource::Resource for PersistProbeScore {}

    /// A resource registered without persistence, to pin the opt-in.
    #[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
    struct PersistProbeOpaque {
        counter: u32,
    }
    impl crate::resource::Resource for PersistProbeOpaque {}

    /// Two builds of one resource, the second a field wider.
    ///
    /// The test process cannot rebuild itself, so a reload is modelled the way
    /// the component suite models it: two distinct Rust types registered under
    /// one persistence name. A real reload produces exactly this state - the
    /// arriving build is a different type to this process, and the name is the
    /// only thing the two generations agree on.
    ///
    /// Deliberately *not* shared resources: a shared name is claimed by one
    /// declaring type and one layout, so two types could never both hold it.
    /// The persistence name is a separate string for precisely this reason.
    #[derive(Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
    struct PersistProbeSettingsV1 {
        volume: f32,
    }
    impl crate::resource::Resource for PersistProbeSettingsV1 {}

    #[derive(Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
    struct PersistProbeSettingsV2 {
        volume: f32,
        brightness: f32,
    }
    impl crate::resource::Resource for PersistProbeSettingsV2 {}

    /// The persistence name both builds of the settings resource register under.
    const PROBE_SETTINGS_NAME: &str = "probe::Settings";

    /// A registered resource's value survives a snapshot and restore.
    ///
    /// Before this existed, `snapshot_resources` did not exist either: a saved
    /// world came back with every resource missing while looking complete.
    #[test]
    fn a_persistable_resource_round_trips_through_a_snapshot() {
        let mut world = World::new();
        world.register_persistable_resource::<PersistProbeScore>();
        world.insert_resource(PersistProbeScore {
            points: 42,
            label: "wave three".to_string(),
        });

        let snapshot = world.snapshot_resources();
        assert_eq!(snapshot.resource_count(), 1, "the resource was captured");

        // A fresh world stands in for the loading process: it declares the
        // resource but has never been given a value.
        let mut loaded = World::new();
        loaded.register_persistable_resource::<PersistProbeScore>();
        assert!(loaded.get_resource::<PersistProbeScore>().is_none());

        loaded.restore_resources(&snapshot);
        assert_eq!(
            loaded.get_resource::<PersistProbeScore>(),
            Some(&PersistProbeScore {
                points: 42,
                label: "wave three".to_string(),
            }),
            "every field comes back"
        );
    }

    /// Registration is what opts a resource into a snapshot.
    #[test]
    fn an_unregistered_resource_stays_out_of_the_snapshot() {
        let mut world = World::new();
        world.register_persistable_resource::<PersistProbeScore>();
        world.insert_resource(PersistProbeScore::default());
        // Inserted, never registered as persistable.
        world.insert_resource(PersistProbeOpaque { counter: 9 });

        let snapshot = world.snapshot_resources();
        assert_eq!(
            snapshot.resource_count(),
            1,
            "only the registered resource is captured"
        );
        assert!(
            snapshot
                .payload(std::any::type_name::<PersistProbeScore>())
                .is_some(),
            "and it is the one that opted in"
        );
    }

    /// A resource that was never inserted contributes no entry.
    ///
    /// "Registered" and "has a value" are different states, and a snapshot
    /// records values. Writing a null would make the loading generation
    /// overwrite whatever it inserted itself with an empty default.
    #[test]
    fn a_registered_resource_with_no_value_is_absent_from_the_snapshot() {
        let mut world = World::new();
        world.register_persistable_resource::<PersistProbeScore>();

        assert_eq!(world.snapshot_resources().resource_count(), 0);
    }

    /// A name the loading generation no longer declares is skipped.
    #[test]
    fn a_resource_the_loader_does_not_declare_is_skipped() {
        let mut world = World::new();
        world.register_persistable_resource::<PersistProbeScore>();
        world.insert_resource(PersistProbeScore {
            points: 3,
            label: String::new(),
        });
        let snapshot = world.snapshot_resources();

        // The loading world declares nothing, as a project that dropped the
        // resource type between builds would.
        let mut loaded = World::new();
        loaded.restore_resources(&snapshot);
        assert!(
            loaded.get_resource::<PersistProbeScore>().is_none(),
            "nothing is resurrected for a type this generation does not own"
        );
    }

    /// A field added since the snapshot arrives at the new type's default.
    #[test]
    fn a_field_added_since_the_resource_snapshot_restores_defaulted() {
        let mut world = World::new();
        world.register_persistable_resource_as::<PersistProbeSettingsV1>(PROBE_SETTINGS_NAME);
        world.insert_resource(PersistProbeSettingsV1 { volume: 0.75 });
        let snapshot = world.snapshot_resources();

        // The next build of the same resource, one field wider, registered
        // under the same persistence name.
        let mut loaded = World::new();
        loaded.register_persistable_resource_as::<PersistProbeSettingsV2>(PROBE_SETTINGS_NAME);
        loaded.restore_resources(&snapshot);

        assert_eq!(
            loaded.get_resource::<PersistProbeSettingsV2>(),
            Some(&PersistProbeSettingsV2 {
                volume: 0.75,
                brightness: 0.0,
            }),
            "the carried field survives and the new one starts defined"
        );
    }

    /// A resource declared by another language round-trips as raw bytes.
    #[test]
    fn a_foreign_resource_round_trips_as_bytes() {
        let mut world = World::new();
        let id = world
            .register_foreign_resource("probe::ForeignTuning", "Probe.Tuning", 8, 4, 0x1234)
            .expect("a valid foreign layout registers");
        assert!(world.register_persistable_foreign_resource("probe::ForeignTuning"));
        world
            .insert_foreign_resource_bytes(id, &[1, 2, 3, 4, 5, 6, 7, 8])
            .expect("the payload is the declared width");

        let snapshot = world.snapshot_resources();
        assert_eq!(snapshot.resource_count(), 1);

        let mut loaded = World::new();
        let loaded_id = loaded
            .register_foreign_resource("probe::ForeignTuning", "Probe.Tuning", 8, 4, 0x1234)
            .expect("the loading generation declares the same shape");
        loaded.restore_resources(&snapshot);

        assert_eq!(
            loaded.foreign_resource_bytes(loaded_id),
            Some(&[1u8, 2, 3, 4, 5, 6, 7, 8][..]),
            "every byte survives the round trip"
        );
    }

    /// A foreign payload whose width disagrees with the live declaration is
    /// refused rather than padded.
    #[test]
    fn a_resized_foreign_resource_payload_is_refused() {
        let mut world = World::new();
        let id = world
            .register_foreign_resource("probe::ForeignResized", "Probe.Resized", 8, 4, 1)
            .expect("a valid foreign layout registers");
        assert!(world.register_persistable_foreign_resource("probe::ForeignResized"));
        world
            .insert_foreign_resource_bytes(id, &[9; 8])
            .expect("the payload is the declared width");
        let snapshot = world.snapshot_resources();

        // The loading generation declares the same resource four bytes wider.
        let mut loaded = World::new();
        let loaded_id = loaded
            .register_foreign_resource("probe::ForeignResized", "Probe.Resized", 12, 4, 2)
            .expect("the wider layout is still valid");
        loaded.restore_resources(&snapshot);

        assert_eq!(
            loaded.foreign_resource_bytes(loaded_id),
            None,
            "an unreconcilable payload leaves the resource unset instead of \
             writing a partly-defined value"
        );
    }

    /// A snapshot is byte-reproducible across two identical captures.
    #[test]
    fn a_resource_snapshot_is_ordered_by_name() {
        let mut world = World::new();
        world.register_persistable_resource::<PersistProbeScore>();
        world.register_persistable_resource_as::<PersistProbeSettingsV1>(PROBE_SETTINGS_NAME);
        world.insert_resource(PersistProbeScore::default());
        world.insert_resource(PersistProbeSettingsV1::default());

        let names: Vec<String> = world
            .snapshot_resources()
            .entries
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "entries come out in name order");
    }

    /// A reshaped resource is migrated across a reload rather than
    /// reinterpreted.
    ///
    /// `rehome_resources` swaps a live resource's function table but never its
    /// bytes, so without this pass the arriving generation would read the old
    /// value's memory through the new shape.
    #[test]
    fn a_reshaped_resource_is_migrated_across_a_reload() {
        let mut world = World::new();
        world.register_persistable_resource_as::<PersistProbeSettingsV1>(PROBE_SETTINGS_NAME);
        world.insert_resource(PersistProbeSettingsV1 { volume: 0.5 });

        // What the host captures immediately before the swap.
        let previous_manifest = world.persist_resource_manifest();
        assert_eq!(previous_manifest.len(), 1);

        // The arriving generation registers the wider shape under the same
        // persistence name, which supersedes the outgoing build's entries.
        world.register_persistable_resource_as::<PersistProbeSettingsV2>(PROBE_SETTINGS_NAME);
        let migrated = world.migrate_changed_persistable_resources(&previous_manifest);

        assert_eq!(migrated, vec![PROBE_SETTINGS_NAME.to_string()]);
        assert_eq!(
            world.get_resource::<PersistProbeSettingsV2>(),
            Some(&PersistProbeSettingsV2 {
                volume: 0.5,
                brightness: 0.0,
            }),
            "the value carries across and the added field is defaulted"
        );
    }

    /// An unchanged resource keeps its value untouched across a reload.
    #[test]
    fn an_unchanged_resource_is_not_migrated() {
        let mut world = World::new();
        world.register_persistable_resource_as::<PersistProbeSettingsV1>(PROBE_SETTINGS_NAME);
        world.insert_resource(PersistProbeSettingsV1 { volume: 0.25 });

        let previous_manifest = world.persist_resource_manifest();
        // The same shape re-registers, as an unedited reload does.
        world.register_persistable_resource_as::<PersistProbeSettingsV1>(PROBE_SETTINGS_NAME);

        assert!(
            world
                .migrate_changed_persistable_resources(&previous_manifest)
                .is_empty(),
            "an unchanged schema takes the fast path"
        );
        assert_eq!(
            world.get_resource::<PersistProbeSettingsV1>(),
            Some(&PersistProbeSettingsV1 { volume: 0.25 }),
            "and the value is left exactly as it was"
        );
    }

    /// Changing only a default *value* is not a schema change.
    ///
    /// The hash normalizes values to kind markers, so editing a literal in a
    /// `Default` impl does not cost every player their stored settings.
    #[test]
    fn a_changed_default_value_does_not_count_as_a_reshape() {
        let mut world = World::new();
        world.register_persistable_resource_as::<PersistProbeSettingsV1>(PROBE_SETTINGS_NAME);
        world.insert_resource(PersistProbeSettingsV1 { volume: 0.9 });
        let first = world.persist_resource_manifest();

        let mut other = World::new();
        other.register_persistable_resource_as::<PersistProbeSettingsV1>(PROBE_SETTINGS_NAME);
        let second = other.persist_resource_manifest();

        assert_eq!(first[0].schema_hash, second[0].schema_hash);
    }
}
