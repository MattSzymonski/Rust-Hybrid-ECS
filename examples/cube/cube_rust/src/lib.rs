//! A spinning cube whose mesh and material are authored as files under `res`.
//!
//! # Responsibilities
//!
//! - Load `res/models/cube.obj` and `res/materials/cube.material` through the
//!   project's asset store, so the editor's Assets tab lists them and its
//!   Inspector edits the material's parameters.
//! - Place the camera and the cube, then spin it around X, Y and Z.
//!
//! # Design
//!
//! The file-authored twin of `cube_no_editor`: same camera, same cube, same
//! spin, but neither the mesh nor the material is built in code. The material
//! names the renderer's PBR shader by guid, so it can only be imported after
//! `pill_master_renderer_data::register` has installed that chain.
//!
//! `init` runs again on every hot reload, against the live world. Every step
//! is idempotent - the asset store hands back an already-loaded path, and the
//! scene is only built into a world that has no camera and no drawn entity
//! yet - so a reload finds the running scene instead of a second copy.

// Standard library
use std::path::{Path, PathBuf};

// External crates
use pill_engine::{
    pill_project, AssetLoader, AssetManager, Engine, Handle, Query, Res, SystemError, Time, World,
};
use pill_master_renderer_data::{
    CameraComponent, Material, Mesh, MeshRendererComponent, TransformComponent,
};

/// The cube's model, relative to `res`.
const CUBE_MESH: &str = "models/cube.obj";

/// The cube's material, relative to `res`.
const CUBE_MATERIAL: &str = "materials/cube.material";

/// Spin speed around X, Y and Z, in radians per second.
const SPIN_RADIANS_PER_SECOND: [f32; 3] = [0.6, 1.0, 0.3];

/// Where the camera stands: on +Z, looking down -Z at the cube.
const CAMERA_TRANSLATION: [f32; 3] = [0.0, 0.0, 5.0];

/// Registers the renderer, loads the cube's files and builds the scene.
#[pill_project]
pub fn init(engine: &mut Engine) -> u32 {
    // Installs the PBR chain whose shader `cube.material` names by guid.
    pill_master_renderer_data::register(engine);
    AssetLoader::set_root(res_directory());

    let (mesh, material) = match load_assets(engine.world_mut()) {
        Ok(assets) => assets,
        Err(error) => {
            pill_engine::tracing::error!("asset loading failed: {error}");
            return 1;
        }
    };
    if let Err(error) = create_scene(engine.world_mut(), &mesh, &material) {
        pill_engine::tracing::error!("scene creation failed: {error}");
        return 1;
    }

    engine.register_system("spin_cube", spin_cube_system);
    0
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

/// Imports the model and the material file, in that order: the material names
/// the shader by guid, and a reference to a shader the store has not imported
/// yet resolves to the renderer's built-in one.
///
/// # Errors
///
/// Returns the engine's message when a file cannot be read, does not decode as
/// its type, or names an asset of another type.
fn load_assets(world: &mut World) -> Result<(Handle<Mesh>, Handle<Material>), String> {
    let mesh = world.load_asset::<Mesh>(CUBE_MESH)?;
    let assets = world
        .get_resource_mut::<AssetManager>()
        .ok_or("the engine AssetManager resource is missing")?;
    let material = assets
        .import_standalone::<Material>(Path::new(CUBE_MATERIAL))
        .map_err(|error| error.to_string())?
        .handle;
    Ok((mesh, material))
}

/// Adds the camera and the cube, unless the world already has them.
///
/// No light entity: the shipped passes carry their own light rig baked into
/// their shaders, so a `DirectionalLightComponent` would travel the scene
/// contract without changing the picture.
///
/// # Errors
///
/// Returns the engine's entity-creation error as text.
fn create_scene(
    world: &mut World,
    mesh: &Handle<Mesh>,
    material: &Handle<Material>,
) -> Result<(), String> {
    if Query::<&CameraComponent>::new(world)
        .iter_mut()
        .next()
        .is_none()
    {
        world
            .create_entity()
            .with(CameraComponent::default())
            .with(TransformComponent {
                translation: CAMERA_TRANSLATION,
                ..Default::default()
            })
            .build()
            .map_err(|error| error.to_string())?;
    }

    if Query::<&MeshRendererComponent>::new(world)
        .iter_mut()
        .next()
        .is_none()
    {
        world
            .create_entity()
            .with(TransformComponent::default())
            .with(
                MeshRendererComponent::builder()
                    .mesh(mesh)
                    .material(material)
                    .build(),
            )
            .build()
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Spins every drawn entity around X, Y and Z, scaled by the frame's delta so
/// the speed does not depend on the frame rate.
fn spin_cube_system(
    time: Res<Time>,
    mut cubes: Query<(&mut TransformComponent, &MeshRendererComponent)>,
) -> Result<(), SystemError> {
    let Some(time) = time.get() else {
        return Ok(());
    };
    let delta = time.delta_seconds();
    let [x, y, z] = SPIN_RADIANS_PER_SECOND;
    let step = glam::Quat::from_euler(glam::EulerRot::XYZ, x * delta, y * delta, z * delta);
    for (mut transform, _) in cubes.iter_mut() {
        let current = glam::Quat::from_array(pill_engine::common_components::rotation_or_identity(
            transform.rotation,
        ));
        transform.rotation = (step * current).normalize().to_array();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Counts the drawn entities, the cameras and the lights.
    fn scene_counts(engine: &mut Engine) -> (usize, usize) {
        let drawn = Query::<&MeshRendererComponent>::new(engine.world_mut())
            .iter_mut()
            .count();
        let cameras = Query::<&CameraComponent>::new(engine.world_mut())
            .iter_mut()
            .count();
        (drawn, cameras)
    }

    #[test]
    fn initialization_loads_the_files_and_builds_one_scene() {
        let mut engine = Engine::new();

        assert_eq!(init(&mut engine), 0);

        assert_eq!(scene_counts(&mut engine), (1, 1));
    }

    /// A hot reload runs `init` again; it must not build a second scene.
    #[test]
    fn initializing_again_keeps_one_scene() {
        let mut engine = Engine::new();
        assert_eq!(init(&mut engine), 0);

        assert_eq!(init(&mut engine), 0);

        assert_eq!(scene_counts(&mut engine), (1, 1));
    }
}
