//! The fixture scene: a handful of fixed entities, one mesh drawn three times
//! and a themed UI layer, for the editor, picking, surface, web, asset and
//! theme suites.
//!
//! # Responsibilities
//!
//! - Define the components the Inspector and the delete/undo steps edit: a tag
//!   the rotation system turns, a component whose fields carry one decorator
//!   each, and a reference to another entity.
//! - Initialize the project: register the renderer, load the bundled model, its
//!   material and the sample map and font, install the game's theme, and build
//!   the scene.
//! - Echo the input a frontend delivers (`controls`), so a suite can tell a
//!   forwarded event from one that never reached the world.
//!
//! # Design
//!
//! Everything here is deliberately static. The three models rotate around their
//! own axes - an animated target for the pause and step checks - and the middle
//! one sits on the camera's axis, so a scripted click lands on it the same way
//! on every run. The entity ids are stable for the same reason: the editor
//! suite's handles are derived from a scene it builds from this file, not from
//! numbers written down here.

// External crates
use pill_engine::{pill_project, Engine, Entity, PillComponent};
use serde::{Deserialize, Serialize};

mod asset_loading;
mod controls;
mod scene;
mod systems;

/// Selects every model that the rotation system animates.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PillComponent)]
#[pill(shared = "scene::TagAlphaComponent", persistable)]
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

/// A reference to another entity, so the Inspector has a reference row to draw
/// (W-0048 Stage 6).
///
/// Not persistable, and it carries nothing serialised: the point is the field's
/// *kind*, which the Inspector turns into a row naming the referenced entity,
/// with a picker, a scene pick and a drop target.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PillComponent)]
pub struct ShowcaseReferenceComponent {
    /// The entity this one points at.
    #[pill(tooltip("The entity this one points at"))]
    pub follow: Entity,
    /// A weight beside the reference, so the section has two rows.
    #[pill(range(0.0, 1.0))]
    pub weight: f32,
}

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
    // Installs the renderer's own PBR chain and points the manager at it. The
    // chain draws materials built for its `pill_pbr` shader, which is what the
    // committed material document names - the editor's scene views and the
    // standalone frontend then draw the same frame through the same chain,
    // and the pick buffer the editor reads is written by a pass that actually
    // draws.
    pill_master_renderer_data::register(engine);
    __pill_register_TagAlphaComponent(engine.world_mut());
    __pill_register_ShowcaseComponent(engine.world_mut());
    __pill_register_ShowcaseReferenceComponent(engine.world_mut());

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

    // The game's UI layer: the egui context, the theme resource and the frame
    // slot the renderer draws through. The code default is the brutalist
    // recipe and names the shipped face; `ui/accent.theme` changes only the
    // accent and the heading size, which is the layering the UI suites assert
    // (`layers=code,file`). `install_with_theme` claims both resource types on
    // every generation and inserts the values only when an earlier one did not
    // leave them behind, so a project reload keeps the same context.
    let mut theme = pill_project_ui::Theme::brutalist();
    theme.fonts.body = Some("fonts/Silkscreen-Regular.ttf".to_string());
    pill_project_ui::install_with_theme(
        engine,
        pill_project_ui::ThemeInstall::new(theme).with_override("ui/accent.theme"),
    );

    engine.register_system("scene_rotation", systems::rotation_system);
    engine.register_system("input_echo", controls::input_echo_system);
    engine.register_system("camera_zoom", controls::camera_zoom_system);
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset_loading::SceneAssets;
    use pill_engine::{Handle, Query};
    use pill_master_renderer_data::{CameraComponent, MeshRendererComponent};

    /// The builder places one camera and three rotating models, and a second
    /// pass adds nothing.
    ///
    /// The builder is driven directly rather than through [`init`]: the host
    /// compiles this project as a generated workspace member, which builds
    /// `pill_master_renderer_data` with `default-features = false`, so the
    /// asset loaders `init` needs are not in that build and it correctly
    /// reports a failed load. The handles are never resolved by the builder, so
    /// invalid ones exercise exactly the shape and the early return that the
    /// suites rely on.
    #[test]
    fn the_builder_creates_one_camera_and_three_rotating_models_once() {
        let mut engine = Engine::new();
        // What `init` does before it loads anything: the renderer's data crate
        // owns the standard components, the three project ones are registered
        // by the generated glue.
        pill_master_renderer_data::register(&mut engine);
        __pill_register_TagAlphaComponent(engine.world_mut());
        __pill_register_ShowcaseComponent(engine.world_mut());
        __pill_register_ShowcaseReferenceComponent(engine.world_mut());
        let world = engine.world_mut();
        let assets = SceneAssets {
            mesh: Handle::INVALID,
            material: Handle::INVALID,
        };
        scene::create(world, assets).expect("the first pass builds the scene");

        let model_count = Query::<&MeshRendererComponent>::new(world)
            .iter_mut()
            .count();
        let rotating_count = Query::<&TagAlphaComponent>::new(world).iter_mut().count();
        let camera_count = Query::<&CameraComponent>::new(world).iter_mut().count();
        assert_eq!(model_count, 3);
        assert_eq!(rotating_count, 3);
        assert_eq!(camera_count, 1);

        // A second pass is what a project reload runs: the builder returns early
        // on the tag the first one left, so nothing is duplicated or replaced.
        scene::create(world, assets).expect("the second pass builds nothing");
        assert_eq!(
            Query::<&MeshRendererComponent>::new(world)
                .iter_mut()
                .count(),
            3
        );
        assert_eq!(Query::<&CameraComponent>::new(world).iter_mut().count(), 1);
    }
}
