//! The components a game places in the world for the master renderer.
//!
//! # Responsibilities
//!
//! - Declare this renderer's own components, one per submodule: the mesh
//!   renderer and the directional light.
//! - Re-export the scene components every renderer shares, so a game imports
//!   all of its render components from this one module: the camera and the
//!   viewport rectangle (renderer contract, `pill_renderer_api`) and the
//!   transform (the engine's).
//! - Register all of them with the world through [`register_components`], field
//!   layout included, for the managed codegen and the editor.
//!
//! # Design
//!
//! The component structs are `#[repr(C)]`, `Serialize` and `Deserialize`,
//! because they are the renderer's public scene contract: projects set them,
//! the managed mirror binds their fields, and the host serialises them when a
//! project reloads.
//!
//! **Every component pins its shared name.** The `PillComponent` derive would
//! otherwise build that name from `module_path!()`, which makes the identity
//! follow the file layout rather than the type: a component moved to another
//! module - or, as these did, to another crate - registers as a *different*
//! component, and the columns a live world already holds for it are orphaned
//! on the next reload. The pinned strings are the names these types carried
//! when they shared a single `component.rs`, kept verbatim through every move.

mod directional_light;
mod mesh_renderer;

pub use directional_light::DirectionalLightComponent;
pub use mesh_renderer::{MeshRendererComponent, MeshRendererComponentBuilder};
pub use pill_engine::common_components::TransformComponent;
pub use pill_renderer_api::components::{CameraComponent, RenderViewport};

// External crates
use pill_engine::World;

/// Registers every component the master renderer draws with, field layouts
/// included.
///
/// The contract's components (the engine's common ones, the transform among
/// them, and the camera) are registered first, then this renderer's own,
/// through the `__pill_register_*` functions the `PillComponent` derives
/// generate. Those registrations are what give the managed codegen and the
/// editor each field's name and offset; without one a component is an opaque
/// blob to both.
pub fn register_components(world: &mut World) {
    pill_renderer_api::components::register_contract_components(world);
    mesh_renderer::register(world);
    directional_light::register(world);
}

#[cfg(test)]
mod tests {
    use super::*;
    use pill_engine::{Component, ComponentId};

    /// Registration records the field layouts the managed codegen and the
    /// editor read. Without them a component is an opaque blob to both, which
    /// is a silent failure everywhere except here.
    #[test]
    fn registering_the_components_records_their_field_layouts() {
        let mut world = World::new();
        register_components(&mut world);

        let layouts = [
            (
                "TransformComponent",
                ComponentId::of::<TransformComponent>(),
            ),
            ("CameraComponent", ComponentId::of::<CameraComponent>()),
            (
                "MeshRendererComponent",
                ComponentId::of::<MeshRendererComponent>(),
            ),
            (
                "DirectionalLightComponent",
                ComponentId::of::<DirectionalLightComponent>(),
            ),
        ];
        for (name, component) in layouts {
            let fields = world
                .component_field_layout(component)
                .unwrap_or_else(|| panic!("{name} has no field layout after registration"));
            assert!(!fields.is_empty(), "{name} has an empty field layout");
        }

        // One row per declared field, in declaration order, which is what the
        // managed mirror is generated from - an empty or blob-like layout would
        // leave it with nothing to bind by name.
        let count = |component| {
            world
                .component_field_layout(component)
                .map(<[_]>::len)
                .unwrap_or(0)
        };
        assert_eq!(count(ComponentId::of::<TransformComponent>()), 3);
        assert_eq!(count(ComponentId::of::<CameraComponent>()), 5);
        assert_eq!(count(ComponentId::of::<MeshRendererComponent>()), 2);
        assert_eq!(count(ComponentId::of::<DirectionalLightComponent>()), 2);
    }

    /// The layout each type declares for the stale-reader check is exactly the
    /// one its registration records. If they disagreed, every query on the
    /// component would be refused as stale.
    #[test]
    fn declared_layouts_match_the_registered_ones() {
        let mut world = World::new();
        register_components(&mut world);
        // The registry hashes the registered descriptors the same way.
        let registered = |component: ComponentId| {
            world
                .component_field_layout(component)
                .filter(|fields| !fields.is_empty())
                .map(pill_engine::component::component_schema_hash)
        };

        assert_eq!(
            TransformComponent::declared_schema_hash(),
            registered(ComponentId::of::<TransformComponent>())
        );
        assert_eq!(
            CameraComponent::declared_schema_hash(),
            registered(ComponentId::of::<CameraComponent>())
        );
        assert_eq!(
            MeshRendererComponent::declared_schema_hash(),
            registered(ComponentId::of::<MeshRendererComponent>())
        );
        assert_eq!(
            DirectionalLightComponent::declared_schema_hash(),
            registered(ComponentId::of::<DirectionalLightComponent>())
        );
        assert!(TransformComponent::declared_schema_hash().is_some());
    }

    /// The pinned shared names are an identity contract, not decoration: they
    /// are what makes the same component recognisable in the host, in a loaded
    /// module, and in a world that outlived the generation which registered it.
    /// Re-deriving one from `module_path!()` would silently orphan live data, so
    /// the exact strings are asserted here, across the move between crates.
    #[test]
    fn the_components_keep_the_shared_names_they_were_pinned_to() {
        assert_eq!(
            TransformComponent::shared_name(),
            Some("pill_master_renderer::component::TransformComponent")
        );
        assert_eq!(
            CameraComponent::shared_name(),
            Some("pill_master_renderer::component::CameraComponent")
        );
        assert_eq!(
            MeshRendererComponent::shared_name(),
            Some("pill_master_renderer::component::MeshRendererComponent")
        );
        assert_eq!(
            DirectionalLightComponent::shared_name(),
            Some("pill_master_renderer::component::DirectionalLightComponent")
        );
    }
}
