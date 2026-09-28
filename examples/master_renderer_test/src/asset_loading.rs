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
    config::pbr_pipeline::{self, PbrMaps},
    AssetLoader, Material, Mesh, Texture, TextureType,
};
use std::path::PathBuf;

/// Asset names, so a reload that runs this twice is refused by the manager with
/// the offending name rather than quietly adding the helmet a second time.
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
/// # Errors
///
/// Returns an error when the manager is missing, when one of the committed
/// files fails to decode, or when an asset name is already taken.
pub(crate) fn load(world: &mut World) -> Result<SceneAssets, Box<dyn std::error::Error>> {
    // The committed content lives beside this crate's manifest.
    AssetLoader::set_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("res"));
    let assets = world
        .get_resource_mut::<AssetManager>()
        .ok_or_else(|| "the engine AssetManager resource is missing".to_owned())?;

    let obj = AssetLoader::Path("models/helmet.obj".into()).load()?;
    let mesh = Mesh::from_obj_bytes("helmet", &obj)?;
    let mesh = assets.add_named(HELMET_MESH, mesh)?;

    let base_color = Texture::new(
        BASE_COLOR,
        TextureType::Color,
        AssetLoader::Path("textures/helmet_basecolor.jpg".into()),
    )?;
    let base_color = assets.add_named(BASE_COLOR, base_color)?;
    let normal = Texture::new(
        NORMAL,
        TextureType::Normal,
        AssetLoader::Path("textures/helmet_normal.jpg".into()),
    )?;
    let normal = assets.add_named(NORMAL, normal)?;
    let metallic_roughness = Texture::new(
        METALLIC_ROUGHNESS,
        TextureType::Color,
        AssetLoader::Path("textures/helmet_metallic_roughness.jpg".into()),
    )?;
    let metallic_roughness = assets.add_named(METALLIC_ROUGHNESS, metallic_roughness)?;
    let emissive = Texture::new(
        EMISSIVE,
        TextureType::Color,
        AssetLoader::Path("textures/helmet_emissive.jpg".into()),
    )?;
    let emissive = assets.add_named(EMISSIVE, emissive)?;

    // The material is built through the chain's own shader. `install` is
    // idempotent, so this is the shader the module already put in the store
    // rather than a second copy, and the slots named here are the ones the
    // shipped PBR shader declares - which is what makes the helmet's material
    // drawable by the chain's geometry pass, and not by some other one.
    let pbr = pbr_pipeline::install(assets)?;
    let material = pbr_pipeline::material(
        assets,
        HELMET_MATERIAL,
        &pbr,
        PbrMaps {
            base_color,
            normal,
            metallic_roughness,
            emissive,
        },
    )?;

    Ok(SceneAssets { mesh, material })
}
