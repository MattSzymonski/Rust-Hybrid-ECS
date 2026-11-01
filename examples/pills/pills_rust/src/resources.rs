//! Loads the mesh, the map and the material the pills draw with.
//!
//! # Responsibilities
//!
//! - Import the pill mesh through its metadata file.
//! - Import the map the pill material binds, then load that material from its
//!   `.material` file.
//! - Hand the scene the handles it draws with ([`SceneAssets`]).
//!
//! # Design
//!
//! The material is a file in `res/materials`, naming the PBR chain's shader and
//! its map by guid, so it can be tuned in the editor and is reloaded live when
//! edited. There is no skybox and no level geometry to load: the pills are the
//! whole scene.
//!
//! Every import is idempotent, so a reload that runs this again gets the
//! assets already loaded rather than a second copy.

// Standard library
use std::path::{Path, PathBuf};

// External crates
use pill_engine::{AssetImport, AssetImportError, AssetManager, Handle, MetadataPolicy, World};
use pill_master_renderer_data::{
    config::pbr_pipeline, AssetLoader, Material, Mesh, Texture, TextureImportSettings, TextureType,
};

/// The mesh, relative to `res`. The path is also the asset's name.
const PILL_MESH: &str = "models/pill.obj";

/// The maps the pill material binds, and how each is read when it has no
/// metadata file yet. Once the file exists, its `texture_type` wins.
const TEXTURES: &[(&str, TextureType)] = &[("textures/grid.png", TextureType::Color)];

/// The pill material's file, relative to `res`.
const PILL_MATERIAL: &str = "materials/pill.material";

/// What the scene needs to draw the pills.
pub(crate) struct SceneAssets {
    /// The mesh every floating pill draws.
    pub pill_mesh: Handle<Mesh>,
    /// The glowing material every floating pill draws with.
    pub pill_material: Handle<Material>,
}

/// This project's `res` directory.
///
/// `file!()` names this source relative to the manifest in an ordinary build,
/// and as an absolute path when the dev host compiles the project through its
/// generated workspace member, whose own manifest lives elsewhere. Joining it
/// onto `CARGO_MANIFEST_DIR` gives this file's real location either way.
pub(crate) fn res_directory() -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(file!());
    source
        .parent()
        .and_then(Path::parent)
        .expect("this file lives in the project's `src`")
        .join("res")
}

/// Loads every asset the pills draw with.
///
/// # Errors
///
/// Returns an error when the asset manager is missing, when a file or its
/// metadata fails to load, or when the PBR chain does not install.
pub(crate) fn load(world: &mut World) -> Result<SceneAssets, Box<dyn std::error::Error>> {
    AssetLoader::set_root(res_directory());
    let assets = world
        .get_resource_mut::<AssetManager>()
        .ok_or("the engine AssetManager resource is missing")?;

    let pill_mesh = import_mesh(assets, PILL_MESH)?;

    // The material names its map by guid, so the map loads first.
    for (path, texture_type) in TEXTURES {
        let request = AssetImport::<Texture>::new(*path, MetadataPolicy::CreateIfMissing)
            .with_initial_settings(TextureImportSettings {
                texture_type: *texture_type,
            });
        assets.import(request)?;
    }

    // ...and so does the shader; `install` returns the chain already installed.
    pbr_pipeline::install(assets)?;
    let pill_material = assets
        .import_standalone::<Material>(Path::new(PILL_MATERIAL))?
        .handle;

    Ok(SceneAssets {
        pill_mesh,
        pill_material,
    })
}

/// Imports the mesh at `path` with its metadata file's settings.
fn import_mesh(assets: &mut AssetManager, path: &str) -> Result<Handle<Mesh>, AssetImportError> {
    let request = AssetImport::<Mesh>::new(path, MetadataPolicy::CreateIfMissing);
    Ok(assets.import(request)?.handle)
}
