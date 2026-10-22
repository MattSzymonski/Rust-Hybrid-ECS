//! Current-ECS port of the original Italian Brainrot example.
//!
//! # Responsibilities
//!
//! - Define the tag component that marks the models the rotation system turns.
//! - Initialize the project: load the bundled assets and build the scene.
//!
//! The scene displays three rotating Chimpanzini Bananini models using lit,
//! unlit, and posterized materials. The original model, texture, shader sources,
//! configuration, and showcase media are kept under `res` and `media`.

// External crates
use pill_engine::{pill_project, Engine, PillComponent};
use serde::{Deserialize, Serialize};

mod asset_loading;
mod scene;
mod systems;

/// Selects every model that the rotation system animates.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PillComponent)]
#[pill(shared = "italian_brainrot::TagAlphaComponent", persistable)]
pub struct TagAlphaComponent;

/// Presentation metadata for the Inspector, one field per decorator.
///
/// The fields carry no meaning to the scene: they exist so the editor suites
/// have a real component whose Inspector section draws a header, a gap, a
/// tooltip, a slider, a clamped drag value and a hidden field, without
/// inventing a second test project.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PillComponent)]
#[pill(persistable)]
pub struct ShowcaseComponent {
    /// Hidden noise: the row must not reach the Inspector, but the value is
    /// stored, serialized and copied like any other field.
    #[pill(hide_in_inspector)]
    pub internal_id: u32,
    /// A header line and a gap above the reading the section opens with.
    #[pill(header("Frame"), space(6.0))]
    pub frame: f32,
    /// A slider over its range, with the explanation on hover.
    #[pill(tooltip("How far the shape leans towards the camera"), range(-1.0, 1.0))]
    pub lean: f32,
    /// A drag value with a floor and no ceiling.
    #[pill(min(0.0))]
    pub scale: f32,
    /// A pair of points: the collection field the Inspector expands into one
    /// row per leaf (`points[0].x`), which is how a struct-array element is
    /// edited (W-0038 Stage 15). The values mean nothing to the scene.
    pub points: [ShowcasePoint; SHOWCASE_POINTS],
}

/// How many showcase points the collection field holds.
pub const SHOWCASE_POINTS: usize = 2;

/// One element of [`ShowcaseComponent::points`]: a value type with a declared
/// layout, so the engine can expand the array into editable leaf rows.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ShowcasePoint {
    pub x: f32,
    pub y: f32,
}

pill_engine::pill_value_type! {
    ShowcasePoint {
        x: f32,
        y: f32,
    }
}

/// Registers the renderer, loads the bundled assets, and creates the scene.
#[pill_project]
pub fn init(engine: &mut Engine) -> u32 {
    pill_master_renderer_data::register(engine);
    // The showcase is the three shading styles, one material each, so this
    // project runs the renderer's built-in frame: a single geometry pass that
    // draws every instance through its own material's shader. `register` only
    // points the manager at the PBR chain as a default - that chain draws
    // materials built for its `pill_pbr` shader alone - so clearing the
    // manager returns the renderer to the built-in pass the styles need.
    engine
        .world_mut()
        .get_resource_mut::<pill_master_renderer_data::RenderingManager>()
        .expect("register inserts the RenderingManager resource")
        .clear();
    __pill_register_TagAlphaComponent(engine.world_mut());
    __pill_register_ShowcaseComponent(engine.world_mut());

    let assets = match asset_loading::load(engine.world_mut()) {
        Ok(assets) => assets,
        Err(error) => {
            pill_engine::tracing::error!("asset loading failed: {error}");
            return 1;
        }
    };

    if let Err(error) = scene::create(engine.world_mut(), assets) {
        pill_engine::tracing::error!("scene creation failed: {error}");
        return 1;
    }

    engine.register_system("italian_brainrot_rotation", systems::rotation_system);
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use pill_engine::Query;
    use pill_master_renderer_data::{CameraComponent, MeshRendererComponent};

    #[test]
    fn initialization_creates_the_original_three_model_scene() {
        let mut engine = Engine::new();
        assert_eq!(init(&mut engine), 0);

        let model_count = Query::<&MeshRendererComponent>::new(engine.world_mut())
            .iter_mut()
            .count();
        let camera_count = Query::<&CameraComponent>::new(engine.world_mut())
            .iter_mut()
            .count();
        assert_eq!(model_count, 3);
        assert_eq!(camera_count, 1);

        // A second initialization is what a project reload runs, so it
        // succeeds: `asset_loading` reuses an asset already under its name
        // instead of adding a duplicate, and the scene builder returns early
        // on the tag the first pass left. Nothing the first scene built is
        // duplicated or replaced.
        assert_eq!(init(&mut engine), 0);
        assert_eq!(
            Query::<&MeshRendererComponent>::new(engine.world_mut())
                .iter_mut()
                .count(),
            3
        );
        assert_eq!(
            Query::<&CameraComponent>::new(engine.world_mut())
                .iter_mut()
                .count(),
            1
        );
    }
}
