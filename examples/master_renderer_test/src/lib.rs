//! Draws the committed DamagedHelmet with a PBR material, through the pipeline
//! the renderer installs for itself.
//!
//! # Responsibilities
//!
//! - Register the renderer, which installs its own PBR chain as the frame to
//!   run, and load the helmet: its mesh, its four PBR maps, and its material
//!   file, which binds them to that chain's shader.
//! - Create the scene: a camera and one spinning helmet.
//! - Let the player turn the helmet and move the camera with the mouse,
//!   keyboard or a gamepad (`controls`).
//!
//! # Design
//!
//! The frame belongs to the renderer, not to this project. `register` installs
//! the PBR chain - geometry into a half-float target, bloom, tonemap, lens - and
//! points the renderer's manager at it, so what is left here is content and
//! scene. Swapping the frame is one `RenderingManager::set_pipeline` call with
//! another pipeline asset, not a change to this file; and the helmet only has to
//! be drawable by whatever chain is running, which is why its material names
//! the chain's shader rather than one of its own.

use pill_engine::{pill_project, Engine, PillComponent};
use serde::{Deserialize, Serialize};

mod asset_loading;
mod controls;
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
    pill_master_renderer_data::register(engine);
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
    engine.register_system("helmet_control", controls::helmet_control_system);
    engine.register_system("camera_zoom", controls::camera_zoom_system);
    engine.register_system("gamepad_rumble", controls::rumble_system);
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use pill_engine::Query;
    use pill_master_renderer_data::{
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

    /// Input a frontend queues reaches this frame's systems: two wheel lines
    /// up bring the camera two zoom steps closer.
    #[test]
    fn scrolling_moves_the_camera_in_the_same_frame() {
        use pill_engine::{InputEvent, ScrollDelta};
        use pill_master_renderer_data::TransformComponent;

        let mut engine = Engine::new();
        assert_eq!(init(&mut engine), 0);
        let camera_distance = |engine: &mut Engine| {
            Query::<(&TransformComponent, &CameraComponent)>::new(engine.world_mut())
                .iter_mut()
                .map(|(transform, _)| transform.translation[2])
                .next()
                .expect("one camera")
        };
        let before = camera_distance(&mut engine);

        engine.push_input_event(InputEvent::MouseWheel {
            delta: ScrollDelta::Lines(glam::Vec2::new(0.0, 2.0)),
        });
        engine.process_frame().expect("frame runs");

        assert_eq!(camera_distance(&mut engine), before - 0.5);
    }

    /// The committed helmet decodes: the OBJ into the mesh the converter
    /// reported, and its base colour map into RGBA pixels. This guards the asset
    /// in `res`, not the loader.
    #[test]
    fn the_committed_helmet_asset_decodes() {
        AssetLoader::set_root(asset_loading::res_directory());

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

    /// A project reload runs the loading code again on the same world. It
    /// must hand back the assets already there: no second copy, no error.
    #[test]
    fn loading_again_reuses_every_asset() {
        use pill_engine::AssetManager;
        use pill_master_renderer_data::Material;

        let mut engine = Engine::new();
        assert_eq!(init(&mut engine), 0);
        let counts = |engine: &mut Engine| {
            let assets = engine
                .world_mut()
                .get_resource::<AssetManager>()
                .expect("init created the manager");
            (
                assets.len::<Mesh>(),
                assets.len::<Texture>(),
                assets.len::<Material>(),
            )
        };
        let before = counts(&mut engine);

        let first = asset_loading::load(engine.world_mut()).expect("first reload");
        let second = asset_loading::load(engine.world_mut()).expect("second reload");

        assert_eq!(counts(&mut engine), before);
        assert_eq!(first.mesh, second.mesh);
        assert_eq!(first.material, second.material);
    }

    /// The helmet's material comes from `res/materials/helmet.material`: its
    /// shader is the PBR chain's, found by the guid the file names, and its
    /// four slots are bound to the loaded maps.
    #[test]
    fn the_helmet_material_comes_from_its_file() {
        use pill_engine::AssetManager;
        use pill_master_renderer_data::MaterialParameter;
        use pill_master_renderer_data::{config::pbr_pipeline, Material, Shader};

        let mut engine = Engine::new();
        assert_eq!(init(&mut engine), 0);
        let assets = engine
            .world_mut()
            .get_resource::<AssetManager>()
            .expect("init created the manager");
        let material = assets
            .get_by_name::<Material>(asset_loading::HELMET_MATERIAL)
            .expect("the material is loaded under its path");

        let shader = assets
            .handle_by_name::<Shader>(pbr_pipeline::SHADER_NAME)
            .expect("the chain installed its shader");
        assert_eq!(material.shader, shader);
        assert_eq!(assets.guid_of(shader), Some(pbr_pipeline::SHADER_GUID));
        for (slot, path) in [
            ("base_color", "textures/helmet_basecolor.jpg"),
            ("normal", "textures/helmet_normal.jpg"),
            (
                "metallic_roughness",
                "textures/helmet_metallic_roughness.jpg",
            ),
            ("emissive", "textures/helmet_emissive.jpg"),
        ] {
            assert_eq!(
                Some(material.textures[slot].texture),
                assets.handle_by_name::<Texture>(path),
                "{slot}"
            );
        }
        assert_eq!(
            material.parameters.get("pbr_roughness"),
            Some(&MaterialParameter::Scalar(1.0))
        );
    }

    /// The maps are read as their metadata files say: the normal map as normal
    /// data, the rest as color.
    #[test]
    fn each_map_is_read_as_its_metadata_says() {
        use pill_engine::AssetManager;

        let mut engine = Engine::new();
        assert_eq!(init(&mut engine), 0);
        let assets = engine
            .world_mut()
            .get_resource::<AssetManager>()
            .expect("init created the manager");
        let expected = [
            ("textures/helmet_basecolor.jpg", TextureType::Color),
            ("textures/helmet_normal.jpg", TextureType::Normal),
            ("textures/helmet_metallic_roughness.jpg", TextureType::Color),
            ("textures/helmet_emissive.jpg", TextureType::Color),
        ];
        for (path, texture_type) in expected {
            let texture = assets
                .get_by_name::<Texture>(path)
                .unwrap_or_else(|| panic!("{path} is loaded under its path"));
            assert_eq!(texture.texture_type, texture_type, "{path}");
            assert!(
                assets
                    .guid_of(assets.handle_by_name::<Texture>(path).unwrap())
                    .is_some(),
                "{path} has a guid"
            );
        }
    }
}
