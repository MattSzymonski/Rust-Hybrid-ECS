//! Owned renderer input extracted from the current ECS after gameplay updates.

use crate::{
    assets::{asset_key, Material, Mesh, Shader, Texture},
    component::{CameraComponent, MeshRendererComponent, TransformComponent},
};
use pill_engine::{Entity, Query, Res, ResMut, Resource, SystemError};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Debug, Default)]
pub struct AssetSnapshot {
    pub revision: u64,
    pub meshes: BTreeMap<u64, Mesh>,
    pub materials: BTreeMap<u64, Material>,
    pub textures: BTreeMap<u64, Texture>,
    pub shaders: BTreeMap<u64, Shader>,
}

impl AssetSnapshot {
    fn from_manager(assets: &pill_engine::AssetManager) -> Self {
        Self {
            revision: assets.revision(),
            meshes: assets
                .iter_handles::<Mesh>()
                .map(|(handle, value)| (asset_key(handle), value.clone()))
                .collect(),
            materials: assets
                .iter_handles::<Material>()
                .map(|(handle, value)| (asset_key(handle), value.clone()))
                .collect(),
            textures: assets
                .iter_handles::<Texture>()
                .map(|(handle, value)| (asset_key(handle), value.clone()))
                .collect(),
            shaders: assets
                .iter_handles::<Shader>()
                .map(|(handle, value)| (asset_key(handle), value.clone()))
                .collect(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RenderInstance {
    pub transform: TransformComponent,
    pub mesh: u64,
    pub material: u64,
}

#[derive(Clone, Debug)]
pub struct RenderFrame {
    pub assets: Arc<AssetSnapshot>,
    pub instances: Vec<RenderInstance>,
    pub camera: CameraComponent,
    pub camera_transform: TransformComponent,
    pub has_camera: bool,
    pub extent: [u32; 2],
    pub sequence: u64,
}

impl Default for RenderFrame {
    fn default() -> Self {
        Self {
            assets: Arc::default(),
            instances: Vec::new(),
            camera: CameraComponent::default(),
            camera_transform: TransformComponent::default(),
            has_camera: false,
            extent: [800, 600],
            sequence: 0,
        }
    }
}

impl Resource for RenderFrame {
    fn shared_name() -> Option<&'static str> {
        Some("pill_master_renderer::frame::RenderFrame")
    }
}

pub fn rendering_system(
    mut frame: ResMut<RenderFrame>,
    assets: Res<pill_engine::AssetManager>,
    mut cameras: Query<(Entity, &TransformComponent, &CameraComponent)>,
    mut objects: Query<(&TransformComponent, &MeshRendererComponent)>,
) -> Result<(), SystemError> {
    let Some(mut frame) = frame.get_mut() else {
        return Ok(());
    };
    let Some(assets) = assets.get() else {
        return Ok(());
    };

    if frame.assets.revision != assets.revision() {
        frame.assets = Arc::new(AssetSnapshot::from_manager(assets));
    }

    frame.sequence = frame.sequence.wrapping_add(1);
    frame.instances.clear();
    frame.has_camera = false;

    let mut selected_priority = i32::MIN;
    let mut selected_entity = u64::MAX;
    for (entity, transform, camera) in cameras.iter_mut() {
        if !camera.enabled
            || (frame.has_camera
                && (camera.priority < selected_priority
                    || (camera.priority == selected_priority && entity.id() >= selected_entity)))
        {
            continue;
        }
        selected_priority = camera.priority;
        selected_entity = entity.id();
        frame.camera = *camera;
        frame.camera_transform = *transform;
        frame.has_camera = true;
    }

    for (transform, renderer) in objects.iter_mut() {
        if assets.contains(renderer.mesh) && assets.contains(renderer.material) {
            frame.instances.push(RenderInstance {
                transform: *transform,
                mesh: asset_key(renderer.mesh),
                material: asset_key(renderer.material),
            });
        }
    }
    frame
        .instances
        .sort_unstable_by_key(|instance| (instance.material, instance.mesh));
    Ok(())
}
