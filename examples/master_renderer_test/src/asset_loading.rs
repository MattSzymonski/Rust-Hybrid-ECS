//! Loads the committed helmet: its mesh, its four PBR maps, and the material
//! the renderer draws it with.
//!
//! # Responsibilities
//!
//! - Import the helmet's mesh and maps through their metadata files.
//! - Load the helmet's material from its own file, after the maps it names.
//! - Hand the scene the handles it draws with ([`SceneAssets`]).
//!
//! # Design
//!
//! The frame is not this project's to declare. `pill_master_renderer_data` installs
//! the PBR chain - a lit pass into a half-float target, bloom, tonemap, and a
//! lens pass onto the surface - and points the renderer at it when the module
//! registers. This file supplies only the geometry, the maps that feed that
//! chain, and the material that binds them to the chain's shader.
//!
//! The mesh and the maps are imported through their metadata files
//! (`res/models/helmet.obj.meta`, `res/textures/*.jpg.meta`): each file holds
//! the asset's guid and how to read it, and is written on the first run when it
//! is missing. The material is a file of its own (`res/materials/helmet.material`)
//! that names the chain's shader and the four maps by guid. Every import is
//! idempotent, so a reload that runs this again gets the assets already loaded
//! rather than a second copy.

use pill_engine::{AssetImport, AssetImportError, AssetManager, Handle, MetadataPolicy, World};
use pill_master_renderer_data::{
    config::pbr_pipeline, AssetLoader, Material, Mesh, Texture, TextureImportSettings, TextureType,
};
use std::path::{Path, PathBuf};

/// Source files, relative to `res`. Each path is also the asset's name in the
/// manager.
const HELMET_MESH: &str = "models/helmet.obj";
const BASE_COLOR: &str = "textures/helmet_basecolor.jpg";
const NORMAL: &str = "textures/helmet_normal.jpg";
const METALLIC_ROUGHNESS: &str = "textures/helmet_metallic_roughness.jpg";
const EMISSIVE: &str = "textures/helmet_emissive.jpg";

/// The helmet's material file, relative to `res`.
pub(crate) const HELMET_MATERIAL: &str = "materials/helmet.material";

/// What the scene needs to draw the helmet.
pub(crate) struct SceneAssets {
    pub mesh: Handle<Mesh>,
    pub material: Handle<Material>,
}

/// This project's `res` directory.
///
/// `file!()` names this source relative to the manifest in an ordinary build,
/// and as an absolute path when the dev host compiles the project through its
/// generated workspace member, whose own manifest lives elsewhere. Joining it
/// onto `CARGO_MANIFEST_DIR` gives this file's real location either way, and
/// `res` sits two levels above it.
pub(crate) fn res_directory() -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(file!());
    source
        .parent()
        .and_then(Path::parent)
        .expect("this file lives in the project's `src`")
        .join("res")
}

/// Decodes the helmet and everything the frame draws it with.
///
/// Safe to run again on a world that already holds them, as a project reload
/// does: every import returns the loaded asset.
///
/// # Errors
///
/// Returns an error when the manager is missing, when one of the committed
/// files or its metadata fails to load, or when the PBR chain does not
/// install.
pub(crate) fn load(world: &mut World) -> Result<SceneAssets, Box<dyn std::error::Error>> {
    AssetLoader::set_root(res_directory());
    let assets = world
        .get_resource_mut::<AssetManager>()
        .ok_or_else(|| "the engine AssetManager resource is missing".to_owned())?;

    let mesh = assets
        .import(AssetImport::<Mesh>::new(
            HELMET_MESH,
            MetadataPolicy::CreateIfMissing,
        ))?
        .handle;

    // The texture types here only seed a missing metadata file; once it
    // exists, its `texture_type` is what the map is read as.
    import_texture(assets, BASE_COLOR, TextureType::Color)?;
    import_texture(assets, NORMAL, TextureType::Normal)?;
    import_texture(assets, METALLIC_ROUGHNESS, TextureType::Color)?;
    import_texture(assets, EMISSIVE, TextureType::Color)?;

    // The material names the chain's shader and the four maps by guid, so
    // both have to be loaded before it is: `install` is idempotent and returns
    // the chain the module already put in the store.
    pbr_pipeline::install(assets)?;
    let material = assets
        .import_standalone::<Material>(Path::new(HELMET_MATERIAL))?
        .handle;

    Ok(SceneAssets { mesh, material })
}

/// Imports the map at `path`, read as `texture_type` when it has no metadata
/// file yet.
fn import_texture(
    assets: &mut AssetManager,
    path: &str,
    texture_type: TextureType,
) -> Result<Handle<Texture>, AssetImportError> {
    let request = AssetImport::<Texture>::new(path, MetadataPolicy::CreateIfMissing)
        .with_initial_settings(TextureImportSettings { texture_type });
    Ok(assets.import(request)?.handle)
}
