//! The scene components that are part of the renderer contract.
//!
//! # Responsibilities
//!
//! - Declare [`CameraComponent`], which [`crate::frame::RenderFrame`] embeds,
//!   and [`RenderViewport`], the rectangle a frontend points a renderer at.
//! - Register the contract's components through
//!   [`register_contract_components`], field layout included.
//!
//! # Design
//!
//! Only what every renderer must understand lives here. A renderer's own
//! components - the master renderer's mesh renderer and directional light, for
//! instance - live in that renderer's data crate, which registers these first
//! and its own after.
//!
//! **The camera pins its shared name.** The `PillComponent` derive would
//! otherwise build it from `module_path!()`, so moving the type would register
//! a *different* component and orphan every column a live world holds for it.

mod camera;
mod viewport;

pub use camera::CameraComponent;
pub use viewport::RenderViewport;

// External crates
use pill_engine::World;

/// Registers the contract's components with the world, field layouts included:
/// the engine's common components (the transform among them) and the camera.
///
/// A renderer's data crate calls this before registering its own components.
pub fn register_contract_components(world: &mut World) {
    pill_engine::common_components::register_common_components(world);
    camera::register(world);
}

#[cfg(test)]
mod tests {
    use super::*;
    use pill_engine::common_components::TransformComponent;
    use pill_engine::{Component, ComponentId};

    /// The contract's components arrive field-described, under their pinned
    /// shared names.
    #[test]
    fn the_contract_components_register_with_their_layouts() {
        let mut world = World::new();
        register_contract_components(&mut world);

        let fields = |component| {
            world
                .component_field_layout(component)
                .map(<[_]>::len)
                .unwrap_or(0)
        };
        assert_eq!(fields(ComponentId::of::<TransformComponent>()), 3);
        assert_eq!(fields(ComponentId::of::<CameraComponent>()), 5);
        assert_eq!(
            CameraComponent::shared_name(),
            Some("pill_master_renderer::component::CameraComponent")
        );
    }
}
