//! The system that turns the world into the frame the renderer draws.
//!
//! # Responsibilities
//!
//! - Collect the frame once per update: the chosen camera, every drawable the
//!   world offers, and the clock the shaders read ([`rendering_system`]).
//! - Resolve the game's pipeline into plain passes, dropping disabled and
//!   stale entries and calling out a chain whose surface pass breaks the
//!   contract.
//!
//! # Design
//!
//! The frame's types - [`RenderFrame`], [`RenderInstance`], [`ResolvedPass`] -
//! are plain data in `pill_renderer_api`, re-exported here; filling them is
//! renderer behaviour and lives in this module, where a renderer reload can
//! replace it. What the system decides rather than reads: the camera choosing
//! among several, the pipeline resolving into plain passes, the drawables
//! resolving into instances that carry their own sort key.

// Standard library
use std::collections::BTreeMap;

// External crates
use pill_core::warn;
use pill_engine::{AssetManager, Component, Entity, Handle, Query, Res, ResMut, SystemError};
pub use pill_renderer_api::frame::{
    CullMode, PassKind, PassTarget, RenderFrame, RenderInstance, ResolvedPass,
};

// Current crate
use crate::{
    assets::asset_key,
    components::{CameraComponent, MeshRendererComponent, TransformComponent},
    resources::RenderingManager,
};

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
            warn!(
                target: pill_core::telemetry::telemetry_target::RENDERING,
                "The pipeline the manager holds no longer resolves; running the built-in pass"
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

        // A pass that names a material draws with it: the material's shader,
        // parameters and textures, under whatever the pass sets itself. Folded
        // in here, where the chain is resolved, so the renderer sees one plain
        // pass and a material edit moves the asset revision that re-resolves it.
        let mut shader = (pass.shader != Handle::INVALID).then(|| asset_key(pass.shader));
        let mut parameters = BTreeMap::new();
        let mut textures = BTreeMap::new();
        if pass.material != Handle::INVALID {
            match assets.get(pass.material) {
                Some(material) => {
                    if shader.is_none() && material.shader != Handle::INVALID {
                        shader = Some(asset_key(material.shader));
                    }
                    parameters.extend(material.parameters.clone());
                    textures.extend(
                        material
                            .textures
                            .iter()
                            .map(|(slot, binding)| (slot.clone(), asset_key(binding.texture))),
                    );
                }
                None => warn!(
                    target: pill_core::telemetry::telemetry_target::RENDERING,
                    "Pass {} names a material that is not loaded",
                    pass.name
                ),
            }
        }
        parameters.extend(pass.parameters.clone());
        textures.extend(
            pass.textures
                .iter()
                .map(|(slot, handle)| (slot.clone(), asset_key(*handle))),
        );

        passes.push(ResolvedPass {
            name: pass.name.clone(),
            shader,
            kind: pass.kind,
            target: pass.target.clone(),
            extra_targets: pass.extra_targets.clone(),
            target_scale: pass.target_scale,
            blend: pass.blend,
            depth_write: pass.depth_write,
            cull: pass.cull,
            parameters,
            inputs: pass.inputs.clone(),
            textures,
            order: pass.order,
        });
    }
    if stale_handles > 0 {
        warn!(
            target: pill_core::telemetry::telemetry_target::RENDERING,
            "Pass chain: {stale_handles} of {} pass handles no longer resolve and are left out",
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
        warn!(
            target: pill_core::telemetry::telemetry_target::RENDERING,
            "Pass chain: {} surface pass(es) at {surface_positions:?}; only the last pass's writes reach the window",
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

/// Shared names of the components [`rendering_system`] reads, for
/// [`RenderCapabilities::consumed_components`](crate::api::RenderCapabilities::consumed_components).
///
/// Derived from the types the system queries rather than written out, so a
/// renamed component can't leave a stale string behind. Keep it in step with
/// the system's parameters.
pub(crate) fn consumed_component_names() -> Vec<String> {
    [
        TransformComponent::shared_name(),
        CameraComponent::shared_name(),
        MeshRendererComponent::shared_name(),
    ]
    .into_iter()
    .flatten()
    .map(str::to_owned)
    .collect()
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
    /// The renderer reports exactly the three components its system queries,
    /// by their pinned shared names.
    #[test]
    fn the_consumed_components_are_the_queried_ones() {
        assert_eq!(
            super::consumed_component_names(),
            [
                "pill_master_renderer::component::TransformComponent",
                "pill_master_renderer::component::CameraComponent",
                "pill_master_renderer::component::MeshRendererComponent",
            ]
        );
    }

    use super::*;
    // `PassKind`, `Shader` and `Texture` are here to build the fixture assets the chain
    // tests need, not by anything in the module itself.
    use crate::{PassKind, RenderPass, RenderingPipeline, Shader, Texture, TextureType};
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
                Shader::new("pbr")
                    .with_wgsl("vertex", "fragment")
                    .with_engine_parameters(true)
                    .with_camera_parameters(true)
                    .build()
                    .expect("the stages are in memory"),
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

    /// A pass that names a material takes the material's shader, parameters and
    /// textures; what the pass sets itself wins over the material's.
    #[test]
    fn a_pass_that_names_a_material_draws_with_it() {
        use crate::{Material, MaterialParameter};

        let mut assets = AssetManager::new();
        let shader = assets
            .add_named(
                "sky_shader",
                Shader::new("sky")
                    .with_wgsl("vertex", "fragment")
                    .build()
                    .expect("the stages are in memory"),
            )
            .expect("a free name");
        let texture = assets
            .add_named(
                "sky_texture",
                Texture::from_rgba("sky", TextureType::Color, vec![0, 0, 0, 255], 1, 1)
                    .expect("a 1x1 RGBA tile"),
            )
            .expect("a free name");
        let material = assets
            .add_named(
                "sky_material",
                Material::builder("sky")
                    .shader(&shader)
                    .texture("sky", &texture)
                    .scalar_parameter("skybox_exposure", 2.0)
                    .scalar_parameter("skybox_rotation", 10.0)
                    .build(),
            )
            .expect("a free name");
        let pass = assets
            .add_named(
                "sky",
                RenderPass::new("sky")
                    .with_kind(PassKind::Skybox)
                    .with_material(material)
                    .with_parameter("skybox_rotation", MaterialParameter::Scalar(90.0)),
            )
            .expect("a free name");
        let pipeline = assets
            .add_named("chain", RenderingPipeline::new().with_pass(pass))
            .expect("a free name");
        let mut manager = RenderingManager::new();
        manager.set_pipeline(pipeline);

        let passes = resolve_passes(&assets, Some(&manager));

        assert_eq!(passes[0].shader, Some(asset_key(shader)));
        assert_eq!(passes[0].textures.get("sky"), Some(&asset_key(texture)));
        assert_eq!(
            passes[0].parameters.get("skybox_exposure"),
            Some(&MaterialParameter::Scalar(2.0))
        );
        assert_eq!(
            passes[0].parameters.get("skybox_rotation"),
            Some(&MaterialParameter::Scalar(90.0)),
            "the pass's own value wins"
        );
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
