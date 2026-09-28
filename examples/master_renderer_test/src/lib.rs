//! Draws the committed DamagedHelmet with a PBR material, through the pipeline
//! the renderer installs for itself.
//!
//! # Responsibilities
//!
//! - Register the renderer, which installs its own PBR chain as the frame to
//!   run, and load the helmet: its mesh, its four PBR maps, and the material
//!   built from them through that chain's shader.
//! - Create the scene: a camera and one spinning helmet.
//!
//! # Design
//!
//! The frame belongs to the renderer, not to this project. `register` installs
//! the PBR chain - geometry into a half-float target, bloom, tonemap, lens - and
//! points the renderer's manager at it, so what is left here is content and
//! scene. Swapping the frame is one `RenderingManager::set_pipeline` call with
//! another pipeline asset, not a change to this file; and the helmet only has to
//! be drawable by whatever chain is running, which is why its material is built
//! through the chain's shader rather than one of its own.

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

/// Registers the renderer, loads the helmet, and creates the scene.
#[pill_project]
pub fn init(engine: &mut Engine) -> u32 {
    // Installs the renderer's own PBR chain and points the renderer at it, so
    // this project has no frame of its own to hand over.
    pill_master_renderer::register(engine);
    __pill_register_TagHelmet(engine.world_mut());

    let assets = match asset_loading::load(engine.world_mut()) {
        Ok(assets) => assets,
        Err(error) => {
            eprintln!("[master_renderer_test] asset loading failed: {error}");
            return 1;
        }
    };

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
