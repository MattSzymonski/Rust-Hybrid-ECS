//! Owned renderer input extracted from the current ECS after gameplay updates.

use crate::{
    assets::{
        asset_key, CullMode, Material, MaterialParameter, Mesh, PassKind, PassTarget, Shader,
        Texture,
    },
    component::{CameraComponent, MeshRendererComponent, TransformComponent},
    resources::RenderingManager,
};
use pill_engine::{Entity, Handle, Query, Res, ResMut, Resource, SystemError};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

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

/// One pass of the chain the renderer runs, with every handle resolved.
///
/// The renderer only ever sees a [`RenderFrame`], and the manager holding the
/// pipeline is a resource of the world, so the chain crosses into the frame as
/// plain values: `shader` is the asset key the renderer finds its GPU shader
/// by, and `None` leaves the choice to the instances, each drawing with the
/// shader its own material names.
#[derive(Clone, Debug)]
pub struct ResolvedPass {
    pub name: String,
    pub shader: Option<u64>,
    pub kind: PassKind,
    pub target: PassTarget,
    /// Further targets the pass writes in the same draw.
    pub extra_targets: Vec<PassTarget>,
    /// Divisor for the size of the targets this pass writes.
    pub target_scale: u32,
    /// Blend over the target instead of replacing it.
    pub blend: bool,
    /// Write depth, or only read it.
    pub depth_write: bool,
    /// Which faces the pass drops.
    pub cull: CullMode,
    pub parameters: HashMap<String, MaterialParameter>,
    pub inputs: HashMap<String, String>,
    /// Committed textures the pass samples, by the texture slot they bind to,
    /// as the asset keys `shader` is a key for.
    pub textures: HashMap<String, u64>,
    pub order: u8,
}

impl ResolvedPass {
    /// A geometry pass over the swapchain that filters no instance.
    ///
    /// This is the chain a project that sets no pipeline runs, and the shape a
    /// project's own single-pass pipeline takes.
    pub fn builtin() -> Self {
        Self {
            name: "builtin.default".to_owned(),
            shader: None,
            kind: PassKind::Geometry,
            target: PassTarget::Surface,
            extra_targets: Vec::new(),
            target_scale: 1,
            blend: false,
            depth_write: true,
            cull: CullMode::Back,
            parameters: HashMap::new(),
            inputs: HashMap::new(),
            textures: HashMap::new(),
            order: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RenderFrame {
    pub assets: Arc<AssetSnapshot>,
    pub instances: Vec<RenderInstance>,
    /// The chain to run this frame, as resolved from the pipeline the game set
    /// in [`RenderingManager`]. Empty asks for a cleared frame and nothing
    /// else, which is what a pipeline with every pass disabled means.
    pub passes: Vec<ResolvedPass>,
    /// Seconds since the frame loop started, and this frame's own duration.
    ///
    /// Carried into the engine's parameters so a shader can animate without the
    /// game handing it a value through a pass parameter, which is read when the
    /// chain is built rather than once a frame.
    pub seconds: f32,
    pub delta_seconds: f32,
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
            passes: Vec::new(),
            seconds: 0.0,
            delta_seconds: 0.0,
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

/// The chain a frame should run, read from the pipeline the game set.
///
/// Disabled passes are dropped, handles that name no live asset are dropped
/// with them, and the rest are ordered by their `order` field; the pipeline's
/// own order breaks ties, so a game lists its passes in the order it wants and
/// only an inserted pass needs an explicit `order`.
fn resolve_passes(
    assets: &pill_engine::AssetManager,
    manager: Option<&RenderingManager>,
) -> Vec<ResolvedPass> {
    let Some(pipeline) = manager
        .and_then(|manager| manager.pipeline())
        .and_then(|pipeline| assets.get(pipeline))
    else {
        return vec![ResolvedPass::builtin()];
    };

    let mut passes: Vec<ResolvedPass> = pipeline
        .passes
        .iter()
        .filter_map(|handle| assets.get(*handle))
        .filter(|pass| pass.enabled)
        .map(|pass| ResolvedPass {
            name: pass.name.clone(),
            shader: (pass.shader != Handle::INVALID).then(|| asset_key(pass.shader)),
            kind: pass.kind,
            target: pass.target.clone(),
            extra_targets: pass.extra_targets.clone(),
            target_scale: pass.target_scale,
            blend: pass.blend,
            depth_write: pass.depth_write,
            cull: pass.cull,
            parameters: pass.parameters.clone(),
            inputs: pass.inputs.clone(),
            textures: pass
                .textures
                .iter()
                .map(|(slot, handle)| (slot.clone(), asset_key(*handle)))
                .collect(),
            order: pass.order,
        })
        .collect();
    passes.sort_by_key(|pass| pass.order);
    passes
}

pub fn rendering_system(
    mut frame: ResMut<RenderFrame>,
    assets: Res<pill_engine::AssetManager>,
    manager: Res<RenderingManager>,
    time: Res<pill_engine::Time>,
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

    // Read every frame rather than cached with the snapshot: a game can set a
    // pipeline, or toggle a pass inside one, at any point between two frames.
    frame.passes = resolve_passes(assets, manager.get());

    frame.sequence = frame.sequence.wrapping_add(1);
    if let Some(time) = time.get() {
        // Accumulated rather than read from the clock: the frame is what a
        // shader is handed, and it should advance by the time the game saw pass
        // while it was filling this frame in.
        frame.delta_seconds = time.delta_seconds();
        frame.seconds += frame.delta_seconds;
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RenderPass, RenderingPipeline, TextureType};
    use pill_engine::AssetManager;
    use std::collections::HashMap;
    #[test]
    fn a_game_with_no_pipeline_runs_the_built_in_pass() {
        let assets = AssetManager::new();

        let passes = resolve_passes(&assets, Some(&RenderingManager::new()));

        assert_eq!(passes.len(), 1);
        assert_eq!(passes[0].name, "builtin.default");
        assert_eq!(passes[0].shader, None);
        assert_eq!(passes[0].kind, PassKind::Geometry);
        assert_eq!(passes[0].target, PassTarget::Surface);
    }

    #[test]
    fn the_chain_runs_in_order_and_leaves_out_disabled_passes() {
        let mut assets = AssetManager::new();
        let second = assets
            .add_named("second", RenderPass::new("second").with_order(1))
            .expect("a free name");
        let first = assets
            .add_named("first", RenderPass::new("first"))
            .expect("a free name");
        let off = assets
            .add_named("off", RenderPass::new("off").with_enabled(false))
            .expect("a free name");
        let pipeline = assets
            .add_named(
                "chain",
                RenderingPipeline::new()
                    .with_pass(second)
                    .with_pass(first)
                    .with_pass(off),
            )
            .expect("a free name");
        let mut manager = RenderingManager::new();
        manager.set_pipeline(pipeline);

        let passes = resolve_passes(&assets, Some(&manager));

        assert_eq!(
            passes
                .iter()
                .map(|pass| pass.name.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"],
            "the disabled pass is left out and order beats pipeline order"
        );
    }

    #[test]
    fn a_pass_that_names_a_shader_carries_its_key() {
        let mut assets = AssetManager::new();
        let shader = assets
            .add_named(
                "pbr",
                Shader::from_wgsl(
                    "pbr",
                    "vertex",
                    "fragment",
                    Vec::new(),
                    HashMap::new(),
                    true,
                    true,
                ),
            )
            .expect("a free name");
        let pass = assets
            .add_named("opaque", RenderPass::new("opaque").with_shader(shader))
            .expect("a free name");
        let pipeline = assets
            .add_named("chain", RenderingPipeline::new().with_pass(pass))
            .expect("a free name");
        let mut manager = RenderingManager::new();
        manager.set_pipeline(pipeline);

        let passes = resolve_passes(&assets, Some(&manager));

        assert_eq!(passes.len(), 1);
        assert_eq!(passes[0].shader, Some(asset_key(shader)));
    }

    #[test]
    fn a_pass_that_binds_a_texture_carries_its_key() {
        let mut assets = AssetManager::new();
        let texture = assets
            .add_named(
                "grain",
                Texture::from_rgba("grain", TextureType::Color, vec![0, 0, 0, 255], 1, 1),
            )
            .expect("a free name");
        let pass = assets
            .add_named(
                "lens",
                RenderPass::new("lens").with_texture("grain", texture),
            )
            .expect("a free name");
        let pipeline = assets
            .add_named("chain", RenderingPipeline::new().with_pass(pass))
            .expect("a free name");
        let mut manager = RenderingManager::new();
        manager.set_pipeline(pipeline);

        let passes = resolve_passes(&assets, Some(&manager));

        assert_eq!(passes.len(), 1);
        assert_eq!(passes[0].textures.get("grain"), Some(&asset_key(texture)));
    }
}
