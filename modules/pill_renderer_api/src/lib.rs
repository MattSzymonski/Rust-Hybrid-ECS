//! The renderer's plain data: what a project draws with, and what the host
//! hands a renderer, with no graphics stack behind it.
//!
//! # Responsibilities
//!
//! - Define the assets a project draws from ([`Mesh`], [`Texture`], [`Shader`],
//!   [`Material`], [`RenderPass`], [`RenderingPipeline`]) and the [`components`]
//!   that place them in the world.
//! - Define the renderer's own pipelines and their WGSL ([`config`]), so every
//!   posture holds the same asset store whether or not anything draws it.
//! - Define the frame a renderer draws ([`RenderFrame`]), the backend contract
//!   a frontend drives ([`PillRenderer`]), and the errors a renderer reports.
//! - Register all of it with an engine through [`register`].
//!
//! # Design
//!
//! This crate never depends on wgpu or winit. The host links it in every
//! posture and projects link it directly; the GPU code lives in the renderer
//! module, the only artifact that links the graphics stack. That split is what
//! lets a headless host load a project that declares cameras, meshes and lights
//! without linking a renderer, and what lets the renderer module be reloaded
//! without the host or the project naming any of its types.
//!
//! Because the host links it, nothing here is reloadable: a change to a type
//! in this crate needs a host restart. It therefore holds data and data-shaped
//! helpers only; renderer behaviour belongs in the module. The WGSL is the
//! exception that stays hot, through the development shader reload
//! ([`shader_reload`], behind the `shader-hot-reload` feature).

/// The renderer contract a frontend drives, plus a headless stub.
pub mod api;

/// The drawable assets - meshes, textures, shaders, materials, passes - and their builders.
pub mod assets;

/// The world-side draw components - transform, camera, mesh renderer, light - and [`register_components`].
pub mod components;

/// Bind group indices, the instance batch size, and the pipelines the renderer ships.
pub mod config;

/// Renderer failures, the `Result` alias and the context helper.
pub mod error;

/// The frame a renderer draws: instances, resolved passes, camera and clock.
pub mod frame;

/// The resource a game sets its pipeline in.
mod rendering_manager;

/// Development shader reload: watches the HLSL sources and updates the shader assets built from them.
#[cfg(feature = "shader-hot-reload")]
pub mod shader_reload;

// External crates
pub use pill_engine::AssetLoader;
use pill_engine::{AssetManager, World};

// Current crate
pub use api::{FrameOutcome, HeadlessRenderer, PillRenderer, RenderCapabilities, RenderMetrics};
pub use assets::{
    Material, MaterialBuilder, MaterialParameter, Mesh, MeshVertex, PassKind, PassTarget,
    RenderPass, RenderingPipeline, Shader, ShaderBuilder, ShaderParameterSlot, ShaderParameterType,
    ShaderTextureSlot, Texture, TextureType,
};
pub use components::*;
pub use error::RendererError;
pub use frame::{RenderFrame, RenderInstance, ResolvedPass};
pub use rendering_manager::RenderingManager;
#[cfg(feature = "shader-hot-reload")]
pub use shader_reload::{ShaderReloadReport, ShaderReloader};

/// Registers the renderer's components, asset types and resources with an
/// engine, and installs its default pipeline; returns zero.
///
/// Registers no system: filling the frame and drawing it is the renderer
/// module's, and a headless engine has neither. Components go in through
/// [`register_components`]; the six asset types are declared so their
/// per-type tables are re-pointed at the artifact still mapped; the
/// [`RenderFrame`] and [`RenderingManager`] resources are only filled in when
/// missing - every step checks what is already there, so a repeated call
/// changes nothing.
///
/// It also installs the PBR pipeline ([`config::pbr_pipeline`]) and points the
/// manager at it, so a project that never declares a frame still draws one.
/// That is the *default*, not a decision: a project that wants a different
/// chain calls [`RenderingManager::set_pipeline`] after registering, and one
/// that wants the renderer's fallback chain clears the manager.
pub fn register(engine: &mut pill_engine::Engine) -> u32 {
    register_components(engine.world_mut());
    // Declare the asset types. Every artifact that registers calls this on
    // every generation, and asset columns outlive the reload, so the
    // declaration is what re-points their per-type tables at an artifact
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
    // The frame the renderer runs when a project declares none: the PBR chain,
    // installed as assets and pointed at by the manager. A project that sets
    // its own pipeline does so after this call, so its choice wins.
    if engine
        .world()
        .get_resource::<RenderingManager>()
        .is_some_and(|manager| manager.pipeline().is_none())
    {
        install_default_pipeline(engine.world_mut());
    }
    0
}

/// Install [`config::pbr_pipeline`] and point the manager at it.
///
/// A failure is warned and not returned: a renderer that could not install the
/// default pipeline still runs, through its built-in chain, and the log says
/// what went wrong. Two things get in here: a name clash with an asset the
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

    /// Registering the data registers no system: drawing is the renderer
    /// module's, and a headless engine must not run any of it.
    #[test]
    fn registering_the_data_registers_no_rendering_system() {
        let mut engine = Engine::new();

        register(&mut engine);

        assert!(engine.is_system_enabled("rendering").is_none());
    }
}
