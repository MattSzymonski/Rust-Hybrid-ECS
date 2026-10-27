//! `#[derive(PillComponent)]` records what each `Handle<T>` field references.
//!
//! # Responsibilities
//!
//! - Prove the derive's generated registration records each handle field's
//!   asset type - `T`'s shared name, or its Rust type name without one - so
//!   the field layer reads the field as a typed asset reference.
//!
//! # Design
//!
//! Registered through the artifact-wide loop, exactly as a module's
//! components are, so the test exercises the generated code and not a
//! hand-written layout.

// External crates
use pill_engine::asset::{Asset, Handle};
use pill_engine::component_field::FieldValue;
use pill_engine::component_registry::register_all_components;
use pill_engine::{ComponentId, PillComponent, World};

/// An asset type with a pinned shared name, as the renderer's are.
struct PinnedMesh;
impl Asset for PinnedMesh {
    fn shared_name() -> Option<&'static str> {
        Some("probe::assets::Mesh")
    }
}

/// An asset type without a shared name.
struct LocalSound;
impl Asset for LocalSound {}

/// A derived component with two handle fields and a scalar between them.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PillComponent)]
struct ReferenceCarrier {
    mesh: Handle<PinnedMesh>,
    volume: f32,
    sound: Handle<LocalSound>,
}

/// Each handle field's asset type is recorded by the derive's registration,
/// and a read of the field names it.
#[test]
fn the_derive_records_each_handle_fields_asset_type() {
    let mut world = World::new();
    register_all_components(&mut world).expect("registration succeeds");

    let component = ComponentId::of::<ReferenceCarrier>();
    assert_eq!(
        world.asset_field_type(component, "mesh"),
        Some("probe::assets::Mesh"),
        "a shared name is the recorded type"
    );
    assert_eq!(
        world.asset_field_type(component, "sound"),
        Some(std::any::type_name::<LocalSound>()),
        "without one, the Rust type name is"
    );
    assert_eq!(world.asset_field_type(component, "volume"), None);

    let entity = world
        .create_entity()
        .with(ReferenceCarrier {
            mesh: Handle::from_raw(4, 1),
            volume: 0.5,
            sound: Handle::INVALID,
        })
        .build()
        .expect("entity builds");
    let name = pill_engine::component::ComponentRegistry::registered_name::<ReferenceCarrier>();
    assert_eq!(
        world.read_component_field(entity, &name, "mesh").unwrap(),
        FieldValue::Asset {
            asset_type: Some("probe::assets::Mesh".to_owned()),
            index: 4,
            generation: 1,
        }
    );
}
