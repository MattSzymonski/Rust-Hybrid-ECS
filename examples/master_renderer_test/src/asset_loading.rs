//! Loads the committed helmet: its mesh, its four PBR maps, and the material
//! the renderer draws it with.
//!
//! The frame is not this project's to declare. `pill_master_renderer` installs
//! its own PBR chain - a lit pass into a half-float target, bloom, tonemap, and a
//! lens pass onto the surface - and points the renderer at it when the module
//! registers. This file supplies only the geometry and the maps that feed that
//! chain, and builds the material through the shader the chain draws with.

use pill_engine::{AssetManager, Handle, World};
use pill_master_renderer::{
    config::pbr_pipeline, AssetLoader, Material, Mesh, Shader, Texture, TextureType,
};
use std::path::PathBuf;

/// Asset names, so a reload reuses what the previous generation loaded instead
/// of adding a second helmet beside it. The `AssetManager` is a world resource
/// and outlives a reload, so these names are a lookup key, not a fresh
/// registration: `load` resolves them before it decodes anything.
const HELMET_MESH: &str = "helmet.mesh";
const HELMET_MATERIAL: &str = "helmet.material";
const BASE_COLOR: &str = "helmet.base_color";
const NORMAL: &str = "helmet.normal";
const METALLIC_ROUGHNESS: &str = "helmet.metallic_roughness";
const EMISSIVE: &str = "helmet.emissive";

/// What the scene needs to draw the helmet.
pub(crate) struct SceneAssets {
    pub mesh: Handle<Mesh>,
    pub material: Handle<Material>,
}

/// Decodes the helmet and everything the frame draws it with.
///
/// Idempotent, because `init` is: a full reload runs this again, and so does
/// the rollback that re-runs the retiring generation's `init` after a failed
/// one. Everything a previous run stored is still in the world's
/// `AssetManager`, so a second run reuses those handles rather than decoding
/// the committed files again - and, just as important, `add_named` is never
/// asked for a name that is already taken, which it refuses.
///
/// # Errors
///
/// Returns an error when the manager is missing or one of the committed files
/// fails to decode. An asset that already exists is not an error; it is the
/// reload path.
pub(crate) fn load(world: &mut World) -> Result<SceneAssets, Box<dyn std::error::Error>> {
    // The committed content lives beside this crate's manifest.
    AssetLoader::set_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("res"));
    let assets = world
        .get_resource_mut::<AssetManager>()
        .ok_or_else(|| "the engine AssetManager resource is missing".to_owned())?;

    // Every asset below is resolved by name first and decoded only when the
    // name is free, so a reload neither re-reads the committed files nor
    // collides with the handles the world still holds.
    let mesh = match assets.handle_by_name::<Mesh>(HELMET_MESH) {
        Some(handle) => handle,
        None => {
            let obj = AssetLoader::Path("models/helmet.obj".into()).load()?;
            let mesh = Mesh::from_obj_bytes("helmet", &obj)?;
            assets.add_named(HELMET_MESH, mesh)?
        }
    };

    let base_color = match assets.handle_by_name::<Texture>(BASE_COLOR) {
        Some(handle) => handle,
        None => {
            let texture = Texture::new(
                BASE_COLOR,
                TextureType::Color,
                AssetLoader::Path("textures/helmet_basecolor.jpg".into()),
            )?;
            assets.add_named(BASE_COLOR, texture)?
        }
    };
    let normal = match assets.handle_by_name::<Texture>(NORMAL) {
        Some(handle) => handle,
        None => {
            let texture = Texture::new(
                NORMAL,
                TextureType::Normal,
                AssetLoader::Path("textures/helmet_normal.jpg".into()),
            )?;
            assets.add_named(NORMAL, texture)?
        }
    };
    let metallic_roughness = match assets.handle_by_name::<Texture>(METALLIC_ROUGHNESS) {
        Some(handle) => handle,
        None => {
            let texture = Texture::new(
                METALLIC_ROUGHNESS,
                TextureType::Color,
                AssetLoader::Path("textures/helmet_metallic_roughness.jpg".into()),
            )?;
            assets.add_named(METALLIC_ROUGHNESS, texture)?
        }
    };
    let emissive = match assets.handle_by_name::<Texture>(EMISSIVE) {
        Some(handle) => handle,
        None => {
            let texture = Texture::new(
                EMISSIVE,
                TextureType::Color,
                AssetLoader::Path("textures/helmet_emissive.jpg".into()),
            )?;
            assets.add_named(EMISSIVE, texture)?
        }
    };

    // The material is built through the chain's own shader, which the chain
    // stores under a published name. `install` is idempotent, so this is the
    // shader the module already put in the store rather than a second copy, and
    // the slots named here are the ones the shipped PBR shader declares - which
    // is what makes the helmet's material drawable by the chain's geometry pass,
    // and not by some other one.
    pbr_pipeline::install(assets)?;
    let material = match assets.handle_by_name::<Material>(HELMET_MATERIAL) {
        Some(handle) => handle,
        None => {
            let shader = assets
                .handle_by_name::<Shader>(pbr_pipeline::SHADER_NAME)
                .ok_or_else(|| "the PBR chain installed without its shader".to_owned())?;
            let material = Material::builder(HELMET_MATERIAL)
                .shader(&shader)
                .texture("base_color", &base_color)
                .texture("normal", &normal)
                .texture("metallic_roughness", &metallic_roughness)
                .texture("emissive", &emissive)
                .color_parameter("pbr_base", [1.0, 1.0, 1.0])
                .scalar_parameter("pbr_roughness", 1.0)
                .scalar_parameter("pbr_metallic", 1.0)
                .color_parameter("pbr_emissive", [1.0, 1.0, 1.0])
                .build();
            assets.add_named(HELMET_MATERIAL, material)?
        }
    };

    Ok(SceneAssets { mesh, material })
}
