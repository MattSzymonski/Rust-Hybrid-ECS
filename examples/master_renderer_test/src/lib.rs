//! Draws the committed DamagedHelmet with a PBR material, through the pass and
//! pipeline assets the project builds.
//!
//! # Responsibilities
//!
//! - Register the renderer and load the helmet, its PBR maps, shader and
//!   material, plus the pass and pipeline that describe the frame.
//! - Hand that pipeline to the renderer's `RenderingManager`.
//! - Create the scene: a camera and one spinning helmet.
//!
//! # Design
//!
//! The frame is described as data. `asset_loading::load` builds a `RenderPass`
//! and a `RenderingPipeline` and stores them as assets like any other, and
//! `RenderingManager::set_pipeline` is the single call that tells the renderer
//! which one to run. Nothing in this project reaches into the renderer to
//! describe a pass in code, which is what would make adding a second pass a
//! change to this file rather than to a renderer struct.

use pill_engine::{pill_project, Engine, PillComponent};
use serde::{Deserialize, Serialize};

mod asset_loading;
mod scene;
mod systems;

/// Marks the entity the rotation system spins.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PillComponent)]
#[pill(shared = "master_renderer_test::TagHelmet", persistable)]
pub struct TagHelmet;

/// Registers the renderer, loads the helmet, hands the renderer its pipeline,
/// and creates the scene.
#[pill_project]
pub fn init(engine: &mut Engine) -> u32 {
    pill_master_renderer::register(engine);
    __pill_register_TagHelmet(engine.world_mut());

    let assets = match asset_loading::load(engine.world_mut()) {
        Ok(assets) => assets,
        Err(error) => {
            eprintln!("[master_renderer_test] asset loading failed: {error}");
            return 1;
        }
    };

    // The one call that connects the project's frame to the renderer.
    let Some(manager) = engine
        .world_mut()
        .get_resource_mut::<pill_master_renderer::RenderingManager>()
    else {
        eprintln!("[master_renderer_test] the renderer's RenderingManager is missing");
        return 1;
    };
    manager.set_pipeline(assets.pipeline);

    if let Err(error) = scene::create(engine.world_mut(), assets) {
        eprintln!("[master_renderer_test] scene creation failed: {error}");
        return 1;
    }

    engine.register_system("helmet_rotation", systems::rotation_system);
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use pill_engine::Query;
    use pill_master_renderer::{
        AssetLoader, CameraComponent, Mesh, MeshRendererComponent, Texture, TextureType,
    };

    #[test]
    fn initialization_creates_one_helmet_and_one_camera() {
        let mut engine = Engine::new();

        assert_eq!(init(&mut engine), 0);

        let model_count = Query::<&MeshRendererComponent>::new(engine.world_mut())
            .iter_mut()
            .count();
        let camera_count = Query::<&CameraComponent>::new(engine.world_mut())
            .iter_mut()
            .count();
        assert_eq!(model_count, 1);
        assert_eq!(camera_count, 1);
    }

    /// The committed helmet decodes: the OBJ into the mesh the converter
    /// reported, and its base colour map into RGBA pixels. This guards the asset
    /// in `res`, not the loader.
    #[test]
    fn the_committed_helmet_asset_decodes() {
        AssetLoader::set_root(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("res"));

        let obj = AssetLoader::Path("models/helmet.obj".into())
            .load()
            .expect("helmet.obj is committed");
        let mesh = Mesh::from_obj_bytes("helmet", &obj).expect("valid OBJ");
        assert_eq!(mesh.vertices.len(), 14_556);
        assert_eq!(mesh.indices.len(), 15_452 * 3);

        let texture = Texture::new(
            "helmet_basecolor",
            TextureType::Color,
            AssetLoader::Path("textures/helmet_basecolor.jpg".into()),
        )
        .expect("helmet_basecolor.jpg is committed and decodable");
        assert!(texture.width > 0 && texture.height > 0);
        assert_eq!(
            texture.rgba.len(),
            texture.width as usize * texture.height as usize * 4
        );
    }
}
