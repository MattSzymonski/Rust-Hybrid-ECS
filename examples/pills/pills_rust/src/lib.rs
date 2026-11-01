//! Two hundred thousand pills drifting through a curl noise field, watched
//! through a free flying camera, with an in-game tuning overlay.
//!
//! # Responsibilities
//!
//! - Own the crate root: `init` (registration, asset import, theme, systems),
//!   the modules, and the items the other files name through `pills_rust::`.
//!
//! # Design
//!
//! The pills are the whole scene: no level geometry and no sky. What the
//! project ships is one model, one map, one material, one font and a theme
//! override, so the loader is the four imports in [`load_assets`] rather than a
//! module of its own.
//!
//! Every other file holds a `#[pill_hot]` system, and a generated fast patch
//! replays such a file's top-level imports into a synthetic crate that links
//! this rlib. Those files must not name `crate::`, so the components and the
//! resource they share are re-exported below and reached as `pills_rust::...`.
//! A patch carries the function's body only, so a system that calls its own
//! file's helpers - the field's maths, the scene's scatter - still falls back to
//! a reload, which the host reports and applies; the overlay calls nothing of
//! its own and patches in place.
//!
//! `init` runs again on every hot reload, on the same world. Every step it
//! takes is idempotent: imports hand back the assets already loaded, the scene
//! is only built into a world that does not hold it yet, and the tuning
//! resource is only inserted when no earlier generation left one behind.

// Standard library
use std::path::{Path, PathBuf};

// External crates
use pill_engine::{
    pill_project, AssetImport, AssetLoader, AssetManager, Engine, Handle, MetadataPolicy, World,
};
use pill_master_renderer_data::{
    config::pbr_pipeline, Material, Mesh, Texture, TextureImportSettings, TextureType,
};

// The crate's own name, so the hot files can name its items as
// `pills_rust::...` in the patch crate too, where `crate::` has no meaning.
extern crate self as pills_rust;

mod camera;
mod curl_noise_system;
mod game;
mod overlay;

// The items the hot files name, reachable in their generated patches through
// `pills_rust::...`.
pub use game::{CameraMovementComponent, CurlNoiseComponent, DemoState, PILL_COUNT};

/// Registers everything the demo uses, loads its assets and builds the scene.
#[pill_project]
pub fn init(engine: &mut Engine) -> u32 {
    // Installs the renderer's PBR chain, whose shader the material names.
    pill_master_renderer_data::register(engine);
    game::register(engine.world_mut());

    let (mesh, material) = match load_assets(engine.world_mut()) {
        Ok(assets) => assets,
        Err(error) => {
            pill_engine::tracing::error!("asset loading failed: {error}");
            return 1;
        }
    };
    if let Err(error) = game::create_scene(engine.world_mut(), mesh, material) {
        pill_engine::tracing::error!("scene creation failed: {error}");
        return 1;
    }

    // The overlay's egui context: installed once and kept across a project
    // reload, so the reloaded overlay finds the same fonts and widget state.
    // `install_with_theme` claims both resource types every generation - the
    // reload releases exactly what the new generation does not re-register -
    // and inserts the values only when no earlier generation left them behind.
    // The frame slot it installs is also what tells the renderer that this
    // world's UI is the game's. The code default is the brutalist recipe; the
    // `.theme` file changes only the accent and the heading size, and the code
    // default names the body font the project ships.
    let mut theme = pill_project_ui::Theme::brutalist();
    theme.fonts.body = Some("fonts/Silkscreen-Regular.ttf".to_string());
    pill_project_ui::install_with_theme(
        engine,
        pill_project_ui::ThemeInstall::new(theme).with_override("ui/accent.theme"),
    );

    engine.register_system("camera", camera::camera_system);
    engine.register_system("curl_noise", curl_noise_system::curl_noise_system);
    engine.register_system("demo_control", game::demo_control_system);
    engine.register_system("tuning_overlay", overlay::tuning_overlay_system);
    engine.register_system("debug_overlay", overlay::debug_overlay_system);
    0
}

/// Imports the pill mesh, the map its material binds and the material itself.
///
/// The material names its map by guid, so the map loads first; the shader it
/// names comes from the PBR chain, so that installs first too. Every import is
/// idempotent, so a reload gets the assets already loaded.
///
/// # Errors
///
/// Returns an error when the asset manager is missing, when a file or its
/// metadata fails to load, or when the chain does not install.
fn load_assets(
    world: &mut World,
) -> Result<(Handle<Mesh>, Handle<Material>), Box<dyn std::error::Error>> {
    AssetLoader::set_root(res_directory());
    let assets = world
        .get_resource_mut::<AssetManager>()
        .ok_or("the engine AssetManager resource is missing")?;

    let mesh = assets
        .import(AssetImport::<Mesh>::new(
            "models/pill.obj",
            MetadataPolicy::CreateIfMissing,
        ))?
        .handle;
    assets.import(
        AssetImport::<Texture>::new("textures/grid.png", MetadataPolicy::CreateIfMissing)
            .with_initial_settings(TextureImportSettings {
                texture_type: TextureType::Color,
            }),
    )?;
    pbr_pipeline::install(assets)?;
    let material = assets
        .import_standalone::<Material>(Path::new("materials/pill.material"))?
        .handle;
    Ok((mesh, material))
}

/// This project's `res` directory.
///
/// `file!()` names this source relative to the manifest in an ordinary build,
/// and as an absolute path when the dev host compiles the project through its
/// generated workspace member, whose own manifest lives elsewhere. Joining it
/// onto `CARGO_MANIFEST_DIR` gives this file's real location either way.
fn res_directory() -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(file!());
    source
        .parent()
        .and_then(Path::parent)
        .expect("this file lives in the project's `src`")
        .join("res")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pill_engine::Query;
    use pill_master_renderer_data::{CameraComponent, MeshRendererComponent};

    /// Counts the entities that draw a mesh, and the cameras.
    fn scene_counts(engine: &mut Engine) -> (usize, usize) {
        let drawn = Query::<&MeshRendererComponent>::new(engine.world_mut())
            .iter_mut()
            .count();
        let cameras = Query::<&CameraComponent>::new(engine.world_mut())
            .iter_mut()
            .count();
        (drawn, cameras)
    }

    /// The scene is every pill, seen by one camera.
    #[test]
    fn initialization_builds_the_scene() {
        let mut engine = Engine::new();

        assert_eq!(init(&mut engine), 0);

        assert_eq!(scene_counts(&mut engine), (PILL_COUNT, 1));
    }

    /// A hot reload runs `init` again; it must not build a second scene.
    #[test]
    fn initializing_again_keeps_one_scene() {
        let mut engine = Engine::new();
        assert_eq!(init(&mut engine), 0);

        assert_eq!(init(&mut engine), 0);

        assert_eq!(scene_counts(&mut engine), (PILL_COUNT, 1));
    }
}
