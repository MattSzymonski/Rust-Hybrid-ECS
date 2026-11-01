//! One spinning cube whose model is an OBJ file and whose material is built in
//! code: no `.meta`, no scene file, no editor layer.
//!
//! # Responsibilities
//!
//! - Register the renderer's data and point it at the simple lit pipeline: one
//!   geometry pass onto the surface, without the PBR chain's post-processing.
//! - Import `res/models/cube.obj` without writing metadata beside it, then
//!   build the tinted material in memory.
//! - Spin the cube around all three axes.
//!
//! # Design
//!
//! The twin of `cube`, which ships the same model *and* a committed `.meta` so
//! the asset has a stable guid for references and the editor can edit it. This
//! project imports the model with [`MetadataPolicy::ReadIfPresent`]: the source
//! decodes under the importer's defaults and its guid lives only in memory, so
//! the `res` tree stays exactly what the repository ships.
//!
//! Setup is idempotent - the asset store hands back an already-loaded path, and
//! entities are looked up before they are created - so a hot reload re-running
//! `init` finds the live scene instead of failing on names that are already
//! taken.

// Standard library
use std::path::{Path, PathBuf};

// External crates
use pill_engine::{
    pill_project, AssetImport, AssetLoader, AssetManager, Engine, Handle, MetadataPolicy, Query,
    Res, SystemError, Time, World,
};
use pill_master_renderer_data::{
    config::simple_pipeline, CameraComponent, Material, Mesh, MeshRendererComponent,
    RenderingManager, Shader, TransformComponent,
};

/// The cube's model, relative to `res`. Deliberately no `cube.obj.meta` beside
/// it: the import below must not create one.
const CUBE_MESH: &str = "models/cube.obj";

/// The material's name, also what makes setup idempotent.
const CUBE_MATERIAL: &str = "cube.material";

/// The original example's tint.
const CUBE_TINT: [f32; 3] = [0.80, 0.80, 0.82];

/// Where the camera stands: on +Z, looking down -Z at the cube.
const CAMERA_TRANSLATION: [f32; 3] = [0.0, 0.0, 5.0];

/// Spin speed around X, Y and Z, in radians per second, as in the original.
const SPIN_RADIANS_PER_SECOND: [f32; 3] = [2.0, 3.5, 1.0];

/// Registers the renderer, imports the model and builds the spin system.
#[pill_project]
pub fn init(engine: &mut Engine) -> u32 {
    pill_master_renderer_data::register(engine);
    // The model is read through the project's `res`; nothing is written back
    // into it.
    AssetLoader::set_root(res_directory());

    let (mesh, material) = match create_assets(engine.world_mut()) {
        Ok(assets) => assets,
        Err(error) => {
            pill_engine::tracing::error!("asset setup failed: {error}");
            return 1;
        }
    };
    if let Err(error) = create_scene(engine.world_mut(), &mesh, &material) {
        pill_engine::tracing::error!("scene creation failed: {error}");
        return 1;
    }

    engine.register_system("rotate_cubes", rotate_cubes_system);
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

/// Installs the simple pipeline, makes it the frame, imports the model file and
/// adds the material (or finds it, after a reload).
///
/// # Errors
///
/// Returns an error when a resource the renderer data registers is missing, when
/// the pipeline fails to install, or when the model does not decode.
fn create_assets(
    world: &mut World,
) -> Result<(Handle<Mesh>, Handle<Material>), Box<dyn std::error::Error>> {
    let assets = world
        .get_resource_mut::<AssetManager>()
        .ok_or("the engine AssetManager resource is missing")?;

    let pipeline = simple_pipeline::install(assets)?;
    let shader = assets
        .handle_by_name::<Shader>(simple_pipeline::SHADER_PATH)
        .ok_or("the simple pipeline installed without its shader")?;

    // `ReadIfPresent` is the point of this project: the source decodes under
    // the importer's defaults, the guid stays in memory, and no `.meta` is
    // written beside the file. An already-loaded path hands back its handle, so
    // a reload imports nothing twice.
    let mesh = assets
        .import(AssetImport::<Mesh>::new(
            CUBE_MESH,
            MetadataPolicy::ReadIfPresent,
        ))?
        .handle;
    let material = match assets.handle_by_name::<Material>(CUBE_MATERIAL) {
        Some(material) => material,
        None => assets.add_named(
            CUBE_MATERIAL,
            Material::builder(CUBE_MATERIAL)
                .shader(&shader)
                .color_parameter("tint", CUBE_TINT)
                .build(),
        )?,
    };

    world
        .get_resource_mut::<RenderingManager>()
        .ok_or("the RenderingManager resource is missing")?
        .set_pipeline(pipeline);
    Ok((mesh, material))
}

/// Adds the camera and the cube, unless the world already has them.
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
fn rotate_cubes_system(
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

    #[test]
    fn initialization_creates_one_cube_and_one_camera_and_is_repeatable() {
        let mut engine = Engine::new();
        assert_eq!(init(&mut engine), 0);
        // A reload runs `init` again on the live world.
        assert_eq!(init(&mut engine), 0);

        let cubes = Query::<&MeshRendererComponent>::new(engine.world_mut())
            .iter_mut()
            .count();
        let cameras = Query::<&CameraComponent>::new(engine.world_mut())
            .iter_mut()
            .count();
        assert_eq!((cubes, cameras), (1, 1));
    }

    /// The import must read the shipped defaults and write nothing: a `.meta`
    /// appearing here would mean the project grew a file the repository does
    /// not carry.
    #[test]
    fn the_model_imports_without_writing_metadata() {
        let mut engine = Engine::new();
        assert_eq!(init(&mut engine), 0);

        let metadata = res_directory().join("models").join("cube.obj.meta");
        assert!(
            !metadata.exists(),
            "the no-editor project wrote {}",
            metadata.display()
        );
    }
}
