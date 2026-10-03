//! Loads the meshes, maps and materials the level draws with.
//!
//! # Responsibilities
//!
//! - Import the pill, pillars and ground meshes through their metadata files.
//! - Import the maps the level's materials bind, then load those materials from
//!   their `.material` files.
//! - Load the night sky (`materials/sky.material`, over the `clear_night.hdr`
//!   panorama) and hand it to the PBR chain's skybox pass.
//! - Hand the scene the handles it draws with ([`SceneAssets`]).
//!
//! # Design
//!
//! Every material is a file in `res/materials`, naming the PBR chain's shader
//! and its maps by guid, so it can be tuned in the editor and is reloaded live
//! when edited. `res/materials` also keeps the materials the original demo
//! built but never drew (fabric, stones, organic, the plain colours, grid and
//! wood); they load the same way when a scene wants them.
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

/// Meshes, relative to `res`. Each path is also the asset's name.
const PILL_MESH: &str = "models/pill.obj";
const PILLARS_MESH: &str = "models/pillars.obj";
const GROUND_MESH: &str = "models/ground.obj";

/// The maps the drawn materials bind, and how each is read when it has no
/// metadata file yet. Once the file exists, its `texture_type` wins.
const TEXTURES: &[(&str, TextureType)] = &[
    ("textures/grid.png", TextureType::Color),
    ("textures/pillars/color.png", TextureType::Color),
    ("textures/pillars/normal.png", TextureType::Normal),
    ("textures/pillars/roughness.png", TextureType::Color),
    ("textures/ground/color.jpg", TextureType::Color),
    ("textures/ground/normal.jpg", TextureType::Normal),
    ("textures/ground/roughness.jpg", TextureType::Color),
    // A Radiance panorama, read as linear half floats for the skybox.
    ("textures/clear_night.hdr", TextureType::Equirect),
];

/// Material files, relative to `res`.
const PILLARS_MATERIAL: &str = "materials/pillars.material";
const GROUND_MATERIAL: &str = "materials/ground.material";
const PILL_MATERIAL: &str = "materials/dark.material";
const SKY_MATERIAL: &str = "materials/sky.material";

/// What the scene needs to draw the level.
pub(crate) struct SceneAssets {
    /// The mesh every floating pill draws.
    pub pill_mesh: Handle<Mesh>,
    /// The glowing material every floating pill draws with.
    pub pill_material: Handle<Material>,
    /// The ring of ancient pillars.
    pub pillars_mesh: Handle<Mesh>,
    /// The pillars' stone material.
    pub pillars_material: Handle<Material>,
    /// The rocky ground under the pillars.
    pub ground_mesh: Handle<Mesh>,
    /// The ground's rock material.
    pub ground_material: Handle<Material>,
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

/// Loads every asset the level draws.
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
    let pillars_mesh = import_mesh(assets, PILLARS_MESH)?;
    let ground_mesh = import_mesh(assets, GROUND_MESH)?;

    // The materials name their maps by guid, so the maps load first.
    for (path, texture_type) in TEXTURES {
        let request = AssetImport::<Texture>::new(*path, MetadataPolicy::CreateIfMissing)
            .with_initial_settings(TextureImportSettings {
                texture_type: *texture_type,
            });
        assets.import(request)?;
    }

    // ...and so does the shader; `install` returns the chain already installed.
    pbr_pipeline::install(assets)?;
    let mut import_material = |path: &str| -> Result<Handle<Material>, AssetImportError> {
        Ok(assets
            .import_standalone::<Material>(Path::new(path))?
            .handle)
    };
    let pillars_material = import_material(PILLARS_MATERIAL)?;
    let ground_material = import_material(GROUND_MATERIAL)?;
    let pill_material = import_material(PILL_MATERIAL)?;
    let sky_material = import_material(SKY_MATERIAL)?;

    // The sky draws behind everything the lit pass drew.
    pbr_pipeline::set_skybox(assets, Some(sky_material))?;

    Ok(SceneAssets {
        pill_mesh,
        pill_material,
        pillars_mesh,
        pillars_material,
        ground_mesh,
        ground_material,
    })
}

/// Imports the mesh at `path` with its metadata file's settings.
fn import_mesh(assets: &mut AssetManager, path: &str) -> Result<Handle<Mesh>, AssetImportError> {
    let request = AssetImport::<Mesh>::new(path, MetadataPolicy::CreateIfMissing);
    Ok(assets.import(request)?.handle)
}
