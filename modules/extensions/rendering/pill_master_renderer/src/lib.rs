//! The wgpu renderer: drawable assets, render components, and the GPU machinery
//! that turns them into frames.
//!
//! # Responsibilities
//!
//! - Defines the assets a project draws from ([`Mesh`], [`Texture`], [`Shader`],
//!   [`Material`], [`RenderPass`], [`RenderingPipeline`]) and the [`components`]
//!   types that place them in the world.
//! - Owns the GPU half: device and surface lifecycle ([`Renderer`]), the
//!   [`resources`] caches, the [`drawers`], and the [`render_queue`] keys that
//!   order a frame's draws.
//! - Installs the renderer on an engine through [`register`], and resolves each
//!   frame's render input in [`frame`].
//!
//! # Design
//!
//! Transferred from the original `pill_renderer` crate. The GPU resource,
//! shader, material, mesh, camera, drawer, and surface code remains in the
//! original module structure; the added [`assets`] and [`frame`] modules adapt
//! that backend to this repository's `AssetManager` and ECS scheduler.
//!
//! The host links this crate directly under its `rendering` feature rather
//! than loading it as a hot-reloadable module, because a renderer is built
//! around a live window surface and driven from the frontend's frame loop.

/// The renderer contract a frontend drives, plus a headless stub.
pub mod api;

/// The drawable assets - meshes, textures, shaders, materials, passes - and their builders.
pub mod assets;

/// The world-side draw components - transform, camera, mesh renderer, light - and [`register_components`].
pub mod components;

/// Bind group indices, the instance batch size, and the two pipelines the renderer ships.
pub mod config;

/// The mesh drawer: batches queued entities and records the instanced draws.
pub mod drawers;

/// Renderer failures, the `Result` alias, and the wgpu validation capture.
pub mod error;

/// Render instances, resolved passes, and the system that builds a frame from the world.
pub mod frame;

/// The per-instance transform the drawer uploads for the vertex shader.
pub mod instance;

/// The frame's chain as GPU objects: a pass pipeline each, and the plan a frame is recorded from.
mod pipeline;

/// The query sets and readbacks behind GPU profiling measurements.
pub mod profiler;

/// Queue items and the packed key fields that order a frame's draws.
pub mod render_queue;

/// The wgpu renderer: device and surface lifecycle, pipelines and submission.
pub mod renderer;

/// The GPU objects behind the asset store, kept level with it one type at a time.
mod rendering_resources_manager;

/// One module per cached GPU resource, re-exported flat for the renderer.
pub mod resources;

/// The window surface, its swapchain, and the lifecycle that keeps them live.
mod surface;

// External crates
use pill_engine::{AssetManager, World};
pub use pill_engine::AssetLoader;

// Current crate
pub use api::{FrameOutcome, HeadlessRenderer, PillRenderer, RenderCapabilities, RenderMetrics};
pub use assets::{
    Material, MaterialBuilder, MaterialParameter, Mesh, MeshVertex, PassKind, PassTarget,
    RenderPass, RenderingPipeline, Shader, ShaderBuilder, ShaderParameterSlot, ShaderParameterType,
    ShaderTextureSlot, Texture, TextureType,
};
pub use components::*;
pub use error::RendererError;
pub use frame::{rendering_system, RenderFrame, RenderInstance, ResolvedPass};
pub use instance::Instance;
pub use renderer::Renderer;
pub use resources::RenderingManager;
pub use surface::RendererWindow;

/// Registers the renderer's components, assets, resources and system with an
/// engine, and returns zero.
///
/// The host calls this once it has attached a [`Renderer`] to a window.
/// Components go in through [`register_components`]; the six asset types are
/// declared so their per-type tables are re-pointed at the generation still
/// mapped; the [`RenderFrame`] and [`RenderingManager`] resources are only
/// filled in when missing, and the post-update `rendering` system is only
/// registered once - every step checks what is already there, so a repeated
/// call changes nothing.
///
/// It also installs the renderer's own PBR pipeline ([`config::pbr_pipeline`])
/// and points the manager at it, so a project that never declares a frame still
/// draws one. That is the *default*, not a decision: a project that wants a
/// different chain calls [`RenderingManager::set_pipeline`] after registering,
/// and one that wants the renderer's fallback chain clears the manager.
pub fn register(engine: &mut pill_engine::Engine) -> u32 {
    register_components(engine.world_mut());
    // Declare the asset types this renderer owns. A project artifact calls this
    // on every generation, and asset columns outlive the reload, so the
    // declaration is what re-points their per-type tables at the generation
    // still mapped.
    engine.world_mut().register_asset::<Mesh>();
    engine.world_mut().register_asset::<Texture>();
    engine.world_mut().register_asset::<Shader>();
    engine.world_mut().register_asset::<Material>();
    engine.world_mut().register_asset::<RenderPass>();
    engine.world_mut().register_asset::<RenderingPipeline>();
    if engine.world().get_resource::<RenderFrame>().is_none() {
        engine.world_mut().insert_resource(RenderFrame::default());
    }
    // The game writes the pipeline here; the renderer reads it back. Inserted
    // here so a project that never sets one still finds the resource.
    if engine.world().get_resource::<RenderingManager>().is_none() {
        engine.world_mut().insert_resource(RenderingManager::new());
    }
    // The frame the renderer runs when a project declares none: its own PBR
    // chain, installed as assets and pointed at by the manager. A project that
    // sets its own pipeline does so after this call, so its choice wins.
    if engine
        .world()
        .get_resource::<RenderingManager>()
        .is_some_and(|manager| manager.pipeline().is_none())
    {
        install_default_pipeline(engine.world_mut());
    }
    if engine.is_system_enabled("rendering").is_none() {
        engine.begin_module_registration(pill_engine::SystemOwner::ENGINE);
        engine.register_post_update_system("rendering", rendering_system);
        engine.end_module_registration();
    }
    0
}

/// Install [`config::pbr_pipeline`] and point the renderer's manager at it.
///
/// A failure is warned and not returned: a renderer that could not install its
/// own default pipeline still runs, through its built-in chain, and the log
/// says what went wrong. Two things get in here: a name clash with an asset the
/// project already owns, and a shader of ours that will not build.
fn install_default_pipeline(world: &mut World) {
    let installed = match world.get_resource_mut::<AssetManager>() {
        Some(assets) => config::pbr_pipeline::install(assets),
        // No store to install into: nothing can be drawn anyway.
        None => return,
    };

    match installed {
        Ok(pipeline) => {
            if let Some(manager) = world.get_resource_mut::<RenderingManager>() {
                manager.set_pipeline(pipeline);
            }
        }
        Err(error) => pill_core::warn!(
            target: pill_core::telemetry::telemetry_target::RENDERING,
            "the default PBR pipeline was not installed: {error}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pill_engine::{Engine, Handle};

    /// The pipeline the manager points at, and the store's view of it.
    fn defaulted_pipeline(engine: &Engine) -> Handle<RenderingPipeline> {
        let manager = engine
            .world()
            .get_resource::<RenderingManager>()
            .expect("register inserts the manager");
        manager
            .pipeline()
            .expect("register points the manager at a pipeline")
    }

    #[test]
    fn registering_installs_the_pbr_pipeline_as_the_default_frame() {
        let mut engine = Engine::new();

        assert_eq!(register(&mut engine), 0);

        // The manager runs the PBR chain, and the store holds it under that
        // chain's name rather than something the project happened to add.
        let pipeline = defaulted_pipeline(&engine);
        let assets = engine
            .world()
            .get_resource::<AssetManager>()
            .expect("the engine inserts the store");
        assert_eq!(
            assets.handle_by_name::<RenderingPipeline>(config::pbr_pipeline::PIPELINE_NAME),
            Some(pipeline)
        );
    }

    /// A reload calls `register` again on a store that already holds the chain.
    #[test]
    fn installing_the_chain_twice_returns_the_same_assets() {
        let mut engine = Engine::new();
        register(&mut engine);
        let first = defaulted_pipeline(&engine);

        assert_eq!(register(&mut engine), 0);

        assert_eq!(defaulted_pipeline(&engine), first);
    }

    /// What a project sets stays set: the default is only for a manager that has
    /// nothing.
    #[test]
    fn a_pipeline_a_project_set_survives_a_later_register() {
        let mut engine = Engine::new();
        register(&mut engine);
        let own = {
            let assets = engine
                .world_mut()
                .get_resource_mut::<AssetManager>()
                .expect("the engine inserts the store");
            config::simple_pipeline::install(assets).expect("a free name")
        };
        engine
            .world_mut()
            .get_resource_mut::<RenderingManager>()
            .expect("register inserts the manager")
            .set_pipeline(own);

        register(&mut engine);

        assert_eq!(defaulted_pipeline(&engine), own);
    }
}
