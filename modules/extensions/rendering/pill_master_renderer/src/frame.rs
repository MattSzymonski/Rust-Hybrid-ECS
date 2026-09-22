//! Owned render packets extracted from the existing ECS.
//!
//! # Responsibilities
//!
//! - Applies pending cooked asset batches and scene lighting settings.
//! - Selects the active camera and copies visible objects into a reusable packet.
//! - Sorts instances by material and mesh for batched GPU submission.
//!
//! # Design
//!
//! Extraction runs in the engine's post-update stage, after deferred commands.
//! No query borrow survives the system call: matrices and component values are
//! copied, and decoded assets are retained through an immutable `Arc` snapshot.
//! This separates world mutation from GPU submission and hot-reload lifetimes.
//!
//! Matrices are column-major. The camera uses a right-handed view looking down
//! -Z and a zero-to-one depth projection, matching the GPU pass.

// Current crate
use crate::component::*;

// External crates
use glam::{Mat4, Quat, Vec3};
use pill_engine::{Entity, Query, Res, ResMut, Resource, SystemError};

// =============================================================================
// Frame Data
// =============================================================================

/// One visible object, detached from its source entity and ECS storage.
#[derive(Clone, Debug)]
pub struct RenderInstance {
    /// Column-major world transform consumed by the instance vertex stream.
    pub model: [[f32; 4]; 4],
    /// Copied asset selection, tint, and fallback PBR factors.
    pub material: PbrRenderableComponent,
}

/// Reusable CPU packet consumed by the host renderer after extraction.
///
/// The host updates `extent` before extraction so projection matches its scene
/// viewport. Clearing `instances` retains allocation capacity between frames.
#[derive(Clone, Debug)]
pub struct RenderFrame {
    /// Decoded asset generation shared with the backend for this packet.
    pub assets: std::sync::Arc<crate::assets::RenderAssets>,
    /// Visible objects sorted by their full material and mesh IDs.
    pub instances: Vec<RenderInstance>,
    /// Column-major projection times view matrix for the selected camera.
    pub view_projection: [[f32; 4]; 4],
    /// World-space camera position used for view-dependent lighting.
    pub camera_position: [f32; 3],
    /// Whether extraction found a valid camera; gates scene and background draws.
    pub has_camera: bool,
    /// Host-supplied scene viewport width and height used for the camera aspect ratio.
    pub extent: [u32; 2],
    /// World-space direction in which the selected light emits rays.
    pub light_direction: [f32; 3],
    /// Linear RGB directional radiance with intensity already applied.
    pub light_color: [f32; 3],
    /// Exposure multiplier passed to the tonemap shader.
    pub exposure: f32,
    /// Wrapping counter advanced by each extraction, even with gameplay paused.
    pub sequence: u64,
    /// Total CPU extraction time, including asset decoding and sorting.
    pub extraction_micros: u64,
    /// Sorting portion of `extraction_micros` for the latest packet.
    pub sort_micros: u64,
    /// Whether the environment background should be drawn when a camera exists.
    pub background: bool,
    /// Texture IDs in background, diffuse IBL, specular IBL, BRDF LUT order.
    pub environment_ids: [u64; 4],
}

impl Default for RenderFrame {
    fn default() -> Self {
        Self {
            assets: Default::default(),
            instances: Vec::new(),
            view_projection: Mat4::IDENTITY.to_cols_array_2d(),
            camera_position: [0.0; 3],
            has_camera: false,
            extent: [800, 600],
            light_direction: [0.4, -0.8, -0.5],
            light_color: [3.0; 3],
            exposure: 1.0,
            sequence: 0,
            extraction_micros: 0,
            sort_micros: 0,
            background: true,
            environment_ids: [0; 4],
        }
    }
}

impl Resource for RenderFrame {}

// =============================================================================
// Transform Helpers
// =============================================================================

/// Compose the component transform in scale-rotation-translation order.
fn model(t: &TransformComponent) -> Mat4 {
    Mat4::from_scale_rotation_translation(
        Vec3::from(t.scale),
        rotation(t.rotation),
        Vec3::from(t.translation),
    )
}

/// Normalize a finite nonzero quaternion; invalid input falls back to identity.
fn rotation(q: [f32; 4]) -> Quat {
    let q = Quat::from_array(q);
    if q.is_finite() && q.length_squared() > 1e-8 {
        q.normalize()
    } else {
        Quat::IDENTITY
    }
}

// =============================================================================
// ECS Extraction
// =============================================================================

/// Extract the current scene without retaining any ECS query references.
///
/// An invalid asset batch is logged and discarded as a whole, preserving the
/// previous snapshot. Missing frame storage makes the system a no-op.
pub fn rendering_system(
    settings: Res<RenderSettings>,
    mut frame: ResMut<RenderFrame>,
    mut requests: ResMut<crate::assets::RenderAssetRequests>,
    mut cameras: Query<(Entity, &TransformComponent, &CameraComponent)>,
    mut objects: Query<(&TransformComponent, &PbrRenderableComponent)>,
    mut lights: Query<(&TransformComponent, &DirectionalLightComponent)>,
) -> Result<(), SystemError> {
    let started = std::time::Instant::now();
    let Some(mut frame) = frame.get_mut() else {
        return Ok(());
    };
    // Step 1: decode into a candidate snapshot so partial batches never become live.
    if let Some(mut requests) = requests.get_mut() {
        let pending = std::mem::take(&mut requests.pending);
        if !pending.is_empty() || requests.replace_all {
            let replace_all = std::mem::take(&mut requests.replace_all);
            let mut replacement = if replace_all {
                crate::assets::RenderAssets::default()
            } else {
                (*frame.assets).clone()
            };
            let mut valid = true;
            for (name, bytes) in pending {
                if let Err(error) = replacement.load(&name, &bytes) {
                    eprintln!("[render] {name}: {error}");
                    valid = false;
                }
            }
            if valid {
                replacement.revision = frame.assets.revision.wrapping_add(1);
                frame.assets = std::sync::Arc::new(replacement);
            }
        }
    }
    // Step 2: reuse packet storage and resolve automatic or explicit environment IDs.
    frame.sequence = frame.sequence.wrapping_add(1);
    frame.instances.clear();
    frame.has_camera = false;
    frame.light_direction = [0.4, -0.8, -0.5];
    frame.light_color = [3.0; 3];
    frame.environment_ids = [
        frame.assets.environment,
        frame.assets.diffuse_ibl,
        frame.assets.specular_ibl,
        frame.assets.brdf_lut,
    ];
    if let Some(settings) = settings.get() {
        frame.exposure = settings.exposure.max(0.0);
        frame.background = settings.background;
        for (i, id) in [
            settings.environment,
            settings.diffuse_ibl,
            settings.specular_ibl,
            settings.brdf_lut,
        ]
        .into_iter()
        .enumerate()
        {
            if id != 0 {
                frame.environment_ids[i] = id;
            }
        }
    }
    // Step 3: select a valid camera deterministically and build its projection.
    let mut priority = i32::MIN;
    let mut selected_id = u64::MAX;
    for (entity, transform, camera) in cameras.iter_mut() {
        if !camera.enabled
            || (frame.has_camera
                && (camera.priority < priority
                    || (camera.priority == priority && entity.id() >= selected_id)))
        {
            continue;
        }
        if transform
            .scale
            .iter()
            .any(|v| !v.is_finite() || v.abs() < 1e-8)
            || !transform.translation.iter().all(|v| v.is_finite())
        {
            continue;
        }
        if !(camera.near.is_finite()
            && camera.far.is_finite()
            && camera.near > 0.0
            && camera.far > camera.near
            && camera.vertical_fov > 0.0
            && camera.vertical_fov < 179.0)
        {
            continue;
        }
        priority = camera.priority;
        selected_id = entity.id();
        frame.has_camera = true;
        let view = model(&transform).inverse();
        let aspect = frame.extent[0].max(1) as f32 / frame.extent[1].max(1) as f32;
        frame.view_projection = (glam::camera::rh::proj::directx::perspective(
            camera.vertical_fov.to_radians(),
            aspect,
            camera.near,
            camera.far,
        ) * view)
            .to_cols_array_2d();
        frame.camera_position = transform.translation;
    }
    // Step 4: copy light and object data while the ECS query borrows are active.
    for (transform, light) in lights.iter_mut() {
        frame.light_direction = (rotation(transform.rotation) * Vec3::NEG_Z).to_array();
        frame.light_color = light.color.map(|c| c * light.intensity);
        break;
    }
    for (transform, material) in objects.iter_mut() {
        if material.visible {
            frame.instances.push(RenderInstance {
                model: model(&transform).to_cols_array_2d(),
                material: *material,
            });
        }
    }
    // Step 5: group adjacent instances for one draw per material/mesh pair.
    let sorting = std::time::Instant::now();
    frame.instances.sort_by_key(|i| {
        crate::render_queue::compose_pbr_render_queue_key(i.material.material, i.material.mesh)
    });
    frame.sort_micros = sorting.elapsed().as_micros() as u64;
    frame.extraction_micros = started.elapsed().as_micros() as u64;
    Ok(())
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use pill_engine::{Commands, Engine};
    /// Extraction sees flushed commands and remains engine-owned across project removal.
    #[test]
    fn extraction_runs_after_deferred_creation_and_while_paused() {
        let mut e = Engine::new();
        crate::register(&mut e);
        crate::register(&mut e);
        e.register_system("spawn", |mut commands: Commands| {
            commands
                .create_entity()
                .with(TransformComponent::default())
                .with(PbrRenderableComponent::default())
                .build();
        });
        e.process_frame().unwrap();
        assert!(e.system_failures().is_empty());
        assert_eq!(
            e.world()
                .get_resource::<RenderFrame>()
                .unwrap()
                .instances
                .len(),
            1
        );
        e.set_systems_paused(true);
        e.process_frame().unwrap();
        assert_eq!(
            e.world()
                .get_resource::<RenderFrame>()
                .unwrap()
                .instances
                .len(),
            1
        );
        assert_eq!(e.world().get_resource::<RenderFrame>().unwrap().sequence, 2);
        assert_eq!(
            e.system_snapshots()
                .iter()
                .filter(|s| s.name == "rendering")
                .count(),
            1
        );
        e.clear_systems_owned_by(pill_engine::SystemOwner::PROJECT);
        e.process_frame().unwrap();
        assert_eq!(e.world().get_resource::<RenderFrame>().unwrap().sequence, 3);
    }

    /// An invalid clipping range must not activate the scene camera.
    #[test]
    fn invalid_and_missing_camera_clear_frame_state() {
        let mut e = Engine::new();
        crate::register(&mut e);
        e.world_mut()
            .create_entity()
            .with(TransformComponent::default())
            .with(CameraComponent {
                near: -1.0,
                ..Default::default()
            })
            .build()
            .unwrap();
        e.process_frame().unwrap();
        assert!(!e.world().get_resource::<RenderFrame>().unwrap().has_camera);
    }
}
