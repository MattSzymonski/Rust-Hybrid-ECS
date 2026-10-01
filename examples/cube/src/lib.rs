//! One spinning cube, ported from the previous engine's `examples/cube`.
//!
//! # Responsibilities
//!
//! - Register the renderer's data and point it at the simple lit pipeline: one
//!   geometry pass onto the surface, without the PBR chain's post-processing.
//! - Build the cube mesh and a tinted material, then a camera and the cube.
//! - Spin the cube around all three axes.
//!
//! # Design
//!
//! The renderer has no cube primitive, so the mesh is built here: 24 vertices,
//! four per face so each face keeps its own flat normal and tangent frame.
//! Setup is idempotent - every asset and entity is looked up before it is
//! created - so a hot reload re-running `init` finds the live scene instead of
//! failing on names that are already taken.

use pill_engine::{pill_project, AssetManager, Engine, Handle, Query, Res, SystemError, Time, World};
use pill_master_renderer_data::{
    config::simple_pipeline, CameraComponent, Material, Mesh, MeshRendererComponent,
    MeshVertex, RenderingManager, Shader, TransformComponent,
};

/// Asset names, also what makes setup idempotent.
const CUBE_MESH: &str = "cube.mesh";
const CUBE_MATERIAL: &str = "cube.material";

/// Edge length of the cube, as in the original example.
const CUBE_SIZE: f32 = 2.0;

/// The original example's tint.
const CUBE_TINT: [f32; 3] = [0.80, 0.80, 0.82];

/// Where the camera stands: on +Z, looking down -Z at the cube.
const CAMERA_TRANSLATION: [f32; 3] = [0.0, 0.0, 5.0];

/// Spin speed around X, Y and Z, in radians per second, as in the original.
const SPIN_RADIANS_PER_SECOND: [f32; 3] = [2.0, 3.5, 1.0];

/// Registers the renderer, builds the scene and the spin system.
#[pill_project]
pub fn init(engine: &mut Engine) -> u32 {
    pill_master_renderer_data::register(engine);

    let (mesh, material) = match create_assets(engine.world_mut()) {
        Ok(assets) => assets,
        Err(error) => {
            eprintln!("[cube] asset setup failed: {error}");
            return 1;
        }
    };
    if let Err(error) = create_scene(engine.world_mut(), &mesh, &material) {
        eprintln!("[cube] scene creation failed: {error}");
        return 1;
    }

    engine.register_system("rotate_cubes", rotate_cubes_system);
    0
}

/// Installs the simple pipeline, makes it the frame, and adds the cube's mesh
/// and material (or finds them, after a reload).
///
/// # Errors
///
/// Returns an error when a resource the renderer data registers is missing, or
/// when the pipeline fails to install or an asset name belongs to another type.
fn create_assets(
    world: &mut World,
) -> Result<(Handle<Mesh>, Handle<Material>), Box<dyn std::error::Error>> {
    let assets = world
        .get_resource_mut::<AssetManager>()
        .ok_or("the engine AssetManager resource is missing")?;

    let pipeline = simple_pipeline::install(assets)?;
    let shader = assets
        .handle_by_name::<Shader>(simple_pipeline::SHADER_NAME)
        .ok_or("the simple pipeline installed without its shader")?;

    let mesh = match assets.handle_by_name::<Mesh>(CUBE_MESH) {
        Some(mesh) => mesh,
        None => assets.add_named(CUBE_MESH, cube_mesh(CUBE_SIZE))?,
    };
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
    if Query::<&CameraComponent>::new(world).iter_mut().next().is_none() {
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
        let current = glam::Quat::from_array(transform.rotation);
        let current = if current.is_finite() && current.length_squared() > 1.0e-8 {
            current.normalize()
        } else {
            glam::Quat::IDENTITY
        };
        transform.rotation = (step * current).normalize().to_array();
    }
    Ok(())
}

/// A cube of edge `size` centred on the origin: four vertices per face, each
/// face wound counter-clockwise seen from outside, as the renderer expects.
fn cube_mesh(size: f32) -> Mesh {
    let half = size / 2.0;
    // Each face as its outward normal and a tangent along it; the bitangent
    // completes a right-handed frame, so `tangent x bitangent = normal`.
    let faces = [
        (glam::Vec3::X, glam::Vec3::NEG_Z),
        (glam::Vec3::NEG_X, glam::Vec3::Z),
        (glam::Vec3::Y, glam::Vec3::X),
        (glam::Vec3::NEG_Y, glam::Vec3::X),
        (glam::Vec3::Z, glam::Vec3::X),
        (glam::Vec3::NEG_Z, glam::Vec3::NEG_X),
    ];
    // Corners in tangent/bitangent units, counter-clockwise, with their UVs.
    let corners = [
        ((-1.0, -1.0), [0.0, 1.0]),
        ((1.0, -1.0), [1.0, 1.0]),
        ((1.0, 1.0), [1.0, 0.0]),
        ((-1.0, 1.0), [0.0, 0.0]),
    ];

    let mut vertices = Vec::with_capacity(faces.len() * corners.len());
    let mut indices = Vec::with_capacity(faces.len() * 6);
    for (normal, tangent) in faces {
        let bitangent = normal.cross(tangent);
        let first = vertices.len() as u32;
        for ((along_tangent, along_bitangent), texture_coordinates) in corners {
            let position =
                (normal + tangent * along_tangent + bitangent * along_bitangent) * half;
            vertices.push(MeshVertex {
                position: position.to_array(),
                texture_coordinates,
                normal: normal.to_array(),
                tangent: tangent.to_array(),
                bitangent: bitangent.to_array(),
            });
        }
        indices.extend([first, first + 1, first + 2, first, first + 2, first + 3]);
    }
    Mesh::from_data("cube", vertices, indices)
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

    #[test]
    fn every_face_winds_counter_clockwise_seen_from_outside() {
        let mesh = cube_mesh(CUBE_SIZE);
        assert_eq!((mesh.vertices.len(), mesh.indices.len()), (24, 36));
        for triangle in mesh.indices.chunks(3) {
            let [a, b, c] = [0, 1, 2].map(|index| {
                glam::Vec3::from_array(mesh.vertices[triangle[index] as usize].position)
            });
            let normal = glam::Vec3::from_array(mesh.vertices[triangle[0] as usize].normal);
            assert!((b - a).cross(c - a).dot(normal) > 0.0);
        }
    }
}
