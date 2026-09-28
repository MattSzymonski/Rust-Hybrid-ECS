//! Owned renderer input extracted from the current ECS after gameplay updates.
//!
//! # Responsibilities
//!
//! - Collect the frame once per update: the chosen camera, every drawable the
//!   world offers, and the clock the shaders read ([`RenderFrame`],
//!   [`rendering_system`]).
//! - Resolve the game's pipeline into plain passes, dropping disabled and
//!   stale entries and calling out a chain whose surface pass breaks the
//!   contract ([`ResolvedPass`]).
//!
//! # Design
//!
//! The renderer only ever sees a [`RenderFrame`]; the assets it draws from it
//! reads straight out of the world's `AssetManager`, borrowed for the call.
//! What crosses here is the part that has to be *decided* rather than read: the
//! camera choosing among several, the pipeline resolving into plain passes, the
//! drawables resolving into instances that carry their own sort key. Assets are
//! left where they live, so a frame never copies the asset store.
//!
//! The frame crosses the artifact boundary - a project's copy of this crate
//! fills it, the host's renderer reads it - so what it carries is plain data:
//! asset keys instead of handles, the camera copied out rather than borrowed.

// Standard library
use std::collections::HashMap;

// External crates
use pill_engine::{
    AssetManager, Entity, Handle, Query, Res, ResMut, Resource, SystemError,
};

// Current crate
use crate::{
    assets::{asset_key, CullMode, MaterialParameter, PassKind, PassTarget},
    components::{CameraComponent, MeshRendererComponent, TransformComponent},
    resources::RenderingManager,
};

/// One drawable the frame collected: where it stands and what it draws with.
///
/// Built from a renderer whose mesh and material both still resolve. The
/// renderer turns each instance into GPU work, batching runs that share a
/// shader, material, and mesh into single instanced draws.
#[derive(Clone, Debug)]
pub struct RenderInstance {
    /// World transform the instance is drawn at.
    pub transform: TransformComponent,
    /// Asset key of the mesh to draw.
    pub mesh: u64,
    /// Asset key of the material to draw it with.
    pub material: u64,
    /// The material's rendering order, resolved here so the renderer can sort
    /// draws without going back to the material asset for each one.
    pub rendering_order: u8,
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
    /// Label used in logs, profiling and error messages.
    pub name: String,
    pub shader: Option<u64>,
    /// What the pass draws.
    pub kind: PassKind,
    /// Where the pass reads and writes.
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
    /// Uniform parameters, packed exactly as a material packs its own.
    pub parameters: HashMap<String, MaterialParameter>,
    /// Offscreen targets the pass samples, by the texture slot they bind to.
    pub inputs: HashMap<String, String>,
    /// Committed textures the pass samples, by the texture slot they bind to,
    /// as the asset keys `shader` is a key for.
    pub textures: HashMap<String, u64>,
    /// Ordering key: lower runs first. Equal orders keep the pipeline's order.
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

/// One frame's renderer input: the instances, the pass chain, the camera, and
/// the clock, extracted from the world and handed to the renderer.
///
/// Everything here crosses as plain owned values, because the frame is what a
/// project's copy of this crate hands the host's renderer: asset keys instead
/// of handles, the camera copied out rather than borrowed. The assets behind
/// those keys are read from the world's `AssetManager` at render time, so the
/// frame stays small however large the project's assets are.
#[derive(Clone, Debug)]
pub struct RenderFrame {
    /// Every drawable the world offered this update, in traversal order.
    pub instances: Vec<RenderInstance>,
    /// The chain to run this frame, as resolved from the pipeline the game set
    /// in [`RenderingManager`]. Empty asks for a cleared frame and nothing
    /// else, which is what a pipeline with every pass disabled means.
    pub passes: Vec<ResolvedPass>,
    /// What the current `passes` were resolved from: the asset revision and the
    /// pipeline's asset key, when the manager holds one.
    ///
    /// Resolution clones every pass's strings and maps, so it repeats only when
    /// one of these moved - and `chain_generation` lets the renderer tell that
    /// the chain it built its pipelines from is still this very chain.
    pub chain_source: Option<(u64, Option<u64>)>,
    /// Bumped whenever `passes` is resolved again.
    pub chain_generation: u64,
    /// Seconds since the frame loop started.
    ///
    /// Carried into the engine's parameters so a shader can animate without the
    /// game handing it a value through a pass parameter, which is read when the
    /// chain is built rather than once a frame.
    pub seconds: f32,
    /// This frame's own duration, the amount `seconds` advances by.
    pub delta_seconds: f32,
    /// Camera the frame renders through, when one is enabled.
    pub camera: CameraComponent,
    /// World transform of that camera.
    pub camera_transform: TransformComponent,
    /// Whether an enabled camera was found; the renderer skips a frame
    /// without one.
    pub has_camera: bool,
    /// Number of this frame, counting from one; packed into the engine
    /// parameters beside the clocks.
    pub sequence: u64,
}

impl Default for RenderFrame {
    fn default() -> Self {
        Self {
            instances: Vec::new(),
            passes: Vec::new(),
            chain_source: None,
            chain_generation: 0,
            seconds: 0.0,
            delta_seconds: 0.0,
            camera: CameraComponent::default(),
            camera_transform: TransformComponent::default(),
            has_camera: false,
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
    let handle = manager.and_then(|manager| manager.pipeline());
    let Some(pipeline) = handle.and_then(|handle| assets.get(handle)) else {
        // A handle that used to resolve but no longer does is a pipeline that
        // died under the game, not one that was never set: the built-in pass
        // keeps the frame drawing either way, and naming the case keeps the
        // log from reading as if the game never asked for a chain.
        if handle.is_some() {
            println!(
                "[render] The pipeline the manager holds no longer resolves; running the built-in pass"
            );
        }
        return vec![ResolvedPass::builtin()];
    };

    let mut stale_handles = 0usize;
    let mut passes = Vec::with_capacity(pipeline.passes.len());
    for pass_handle in &pipeline.passes {
        let Some(pass) = assets.get(*pass_handle) else {
            stale_handles += 1;
            continue;
        };
        // A disabled pass is the game's own choice, not a fault to report.
        if !pass.enabled {
            continue;
        }
        passes.push(ResolvedPass {
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
        });
    }
    if stale_handles > 0 {
        println!(
            "[render] Pass chain: {stale_handles} of {} pass handles no longer resolve and are left out",
            pipeline.passes.len()
        );
    }
    passes.sort_by_key(|pass| pass.order);

    // Contract from `RenderPass`: one pass writes the surface and it runs last,
    // or the window shows something other than what the chain produced. Named
    // here, where the chain changes, rather than once a frame.
    let surface_positions: Vec<usize> = passes
        .iter()
        .enumerate()
        .filter(|(_, pass)| matches!(pass.target, PassTarget::Surface))
        .map(|(index, _)| index)
        .collect();
    let surface_is_last = surface_positions.len() == 1 && surface_positions[0] + 1 == passes.len();
    if !surface_positions.is_empty() && !surface_is_last {
        println!(
            "[render] Pass chain: {} surface pass(es) at {surface_positions:?}; only the last pass's writes reach the window",
            surface_positions.len()
        );
    }
    passes
}

/// The camera a frame renders through, chosen out of every enabled one.
///
/// Highest `priority` wins; equal priorities go to the lowest entity id, so
/// the choice cannot wander with traversal order while the same cameras exist.
/// `None` means no camera is enabled, which is a frame the renderer skips.
fn pick_camera(
    cameras: impl IntoIterator<Item = (u64, CameraComponent, TransformComponent)>,
) -> Option<(CameraComponent, TransformComponent)> {
    let mut selected: Option<(u64, CameraComponent, TransformComponent)> = None;
    for (entity, camera, transform) in cameras {
        if !camera.enabled {
            continue;
        }
        let better = match &selected {
            Some((selected_entity, selected_camera, _)) => {
                camera.priority > selected_camera.priority
                    || (camera.priority == selected_camera.priority && entity < *selected_entity)
            }
            None => true,
        };
        if better {
            selected = Some((entity, camera, transform));
        }
    }
    selected.map(|(_, camera, transform)| (camera, transform))
}

/// Fills the frame from the current world, once per update.
///
/// Registered as the post-update `rendering` system, so gameplay has already
/// written the components it reads. The resolved chain is refreshed only when
/// what it was derived from moved - the asset revision and the pipeline's key -
/// so most frames reuse the last one's work. A world with no frame resource, no
/// assets, or no enabled camera is not an error: the frame simply carries less,
/// and the renderer clears or skips it.
pub fn rendering_system(
    mut frame: ResMut<RenderFrame>,
    assets: Res<AssetManager>,
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

    // The manager is re-read every frame - a game can set a pipeline, or toggle
    // a pass inside one, at any point between two frames - but the chain itself
    // is resolved again only when the pipeline handle or the revision moved.
    // Resolution clones every pass's strings and maps, and the renderer uses
    // the generation it produces to recognise the chain it already built from.
    let chain_source = (
        assets.revision(),
        manager
            .get()
            .and_then(|manager| manager.pipeline())
            .map(asset_key),
    );
    if frame.chain_source != Some(chain_source) {
        frame.passes = resolve_passes(assets, manager.get());
        frame.chain_source = Some(chain_source);
        frame.chain_generation = frame.chain_generation.wrapping_add(1);
    }

    frame.sequence = frame.sequence.wrapping_add(1);
    if let Some(time) = time.get() {
        // Accumulated rather than read from the clock: the frame is what a
        // shader is handed, and it should advance by the time the game saw pass
        // while it was filling this frame in.
        frame.delta_seconds = time.delta_seconds();
        frame.seconds += frame.delta_seconds;
    }
    frame.instances.clear();
    // The camera is picked out of every enabled one; see [`pick_camera`] for
    // what happens when several are enabled at once.
    let selected = pick_camera(
        cameras
            .iter_mut()
            .map(|(entity, transform, camera)| (entity.id(), *camera, *transform)),
    );
    frame.has_camera = selected.is_some();
    if let Some((camera, transform)) = selected {
        frame.camera = camera;
        frame.camera_transform = transform;
    }

    for (transform, renderer) in objects.iter_mut() {
        // Handles with nothing behind them are dropped here, so a dangling
        // handle draws nothing instead of failing the frame. The material is
        // read rather than merely checked because the draw order it carries
        // rides along with the instance; the renderer imposes that order when
        // it composes the queue, so what is left here stays in traversal order.
        let Some(material) = assets.get(renderer.material) else {
            continue;
        };
        if !assets.contains(renderer.mesh) {
            continue;
        }
        frame.instances.push(RenderInstance {
            transform: *transform,
            mesh: asset_key(renderer.mesh),
            material: asset_key(renderer.material),
            rendering_order: material.rendering_order,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    // `Shader` and `Texture` are here to build the fixture assets the chain
    // tests need, not by anything in the module itself.
    use crate::{RenderPass, RenderingPipeline, Shader, Texture, TextureType};
    use pill_engine::AssetManager;

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
                    Vec::new(),
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
                Texture::from_rgba("grain", TextureType::Color, vec![0, 0, 0, 255], 1, 1)
                    .expect("a 1x1 RGBA tile"),
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

    fn camera(priority: i32, enabled: bool, vertical_fov: f32) -> CameraComponent {
        CameraComponent {
            priority,
            enabled,
            vertical_fov,
            ..CameraComponent::default()
        }
    }

    #[test]
    fn the_highest_priority_camera_is_selected() {
        let selected = pick_camera([
            (1, camera(0, true, 60.0), TransformComponent::default()),
            (2, camera(5, true, 42.0), TransformComponent::default()),
        ]);

        assert_eq!(selected.map(|(camera, _)| camera.vertical_fov), Some(42.0));
    }

    #[test]
    fn equal_priorities_break_ties_by_the_lowest_entity_id() {
        let selected = pick_camera([
            (7, camera(0, true, 60.0), TransformComponent::default()),
            (3, camera(0, true, 42.0), TransformComponent::default()),
            (5, camera(0, true, 20.0), TransformComponent::default()),
        ]);

        assert_eq!(selected.map(|(camera, _)| camera.vertical_fov), Some(42.0));
    }

    #[test]
    fn a_disabled_camera_is_never_selected() {
        let selected = pick_camera([
            (1, camera(9, false, 42.0), TransformComponent::default()),
            (2, camera(0, true, 60.0), TransformComponent::default()),
        ]);

        assert_eq!(selected.map(|(camera, _)| camera.vertical_fov), Some(60.0));
        assert!(
            pick_camera([(1, camera(9, false, 42.0), TransformComponent::default())]).is_none()
        );
    }

    #[test]
    fn a_pipeline_handle_that_no_longer_resolves_falls_back_to_the_built_in_pass() {
        let mut assets = AssetManager::new();
        let pass = assets
            .add_named("only", RenderPass::new("only"))
            .expect("a free name");
        let pipeline = assets
            .add_named("chain", RenderingPipeline::new().with_pass(pass))
            .expect("a free name");
        let mut manager = RenderingManager::new();
        manager.set_pipeline(pipeline);
        assets.remove(pipeline);

        let passes = resolve_passes(&assets, Some(&manager));

        assert_eq!(passes.len(), 1);
        assert_eq!(passes[0].name, "builtin.default");
    }

    #[test]
    fn a_pass_handle_that_no_longer_resolves_is_left_out_of_the_chain() {
        let mut assets = AssetManager::new();
        let pass = assets
            .add_named("gone", RenderPass::new("gone"))
            .expect("a free name");
        let pipeline = assets
            .add_named("chain", RenderingPipeline::new().with_pass(pass))
            .expect("a free name");
        let mut manager = RenderingManager::new();
        manager.set_pipeline(pipeline);
        assets.remove(pass);

        let passes = resolve_passes(&assets, Some(&manager));

        assert!(
            passes.is_empty(),
            "a chain whose only pass is gone resolves to nothing"
        );
    }
}
