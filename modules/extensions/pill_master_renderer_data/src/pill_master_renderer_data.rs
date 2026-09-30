//! The master renderer's data: everything a game says to this renderer, with
//! no graphics stack behind it.
//!
//! # Responsibilities
//!
//! - Define the assets a project draws from ([`Mesh`], [`Texture`], [`Shader`],
//!   [`Material`], [`RenderPass`], [`RenderingPipeline`]) and the [`components`]
//!   that place them in the world.
//! - Define this renderer's pipelines and their shaders ([`config`]), and the
//!   resource a game selects one in ([`RenderingManager`]).
//! - Register all of it with an engine through [`register`], and install the
//!   default pipeline.
//!
//! # Design
//!
//! This crate never depends on wgpu or winit. It is one of the master
//! renderer's two crates: this one holds the data, `pill_master_renderer` holds
//! the GPU code and is the only artifact that links the graphics stack. That
//! split lets a headless build store a game's cameras, meshes and lights
//! without linking a renderer, and lets the GPU module be reloaded without
//! registering any data of its own.
//!
//! The contract every renderer shares - the backend trait, the frame, the pass
//! vocabulary, the camera - lives in `pill_renderer_api`, which this crate
//! builds on; the types a game needs from it are re-exported here.
//!
//! The shaders live at the crate root (`shaders/`), not under `src/`, so a
//! shader edit is not a Rust source edit. In development the host watches and
//! re-cooks them, and hands each result to [`shader_hot_reload`]'s export,
//! compiled only into a loaded module (`module-abi`).

/// The drawable assets - meshes, textures, shaders, materials, passes - and their builders.
pub mod assets;

/// The world-side draw components and [`register_components`].
pub mod components;

/// The C# bridge's asset functions, offered to the host by name.
pub mod csharp_assets;

/// Bind group indices, the instance batch size, and the pipelines the renderer ships.
pub mod config;

/// The resource a game sets its pipeline in.
mod rendering_manager;

/// Development shader reload: puts WGSL the host re-cooked into the shader assets built from it.
#[cfg(any(test, feature = "module-abi"))]
pub mod shader_hot_reload;

// External crates
pub use pill_engine::AssetLoader;
use pill_engine::{pill_module, AssetManager, Engine, World};
pub use pill_renderer_api::frame::{
    CullMode, MaterialParameter, PassKind, PassTarget, RenderFrame,
};

// Current crate
pub use assets::{
    Material, MaterialBuilder, MaterialTexture, Mesh, MeshVertex, RenderPass, RenderingPipeline,
    Shader, ShaderBuilder, ShaderParameterSlot, ShaderParameterType, ShaderTextureSlot, Texture,
    TextureType,
};
pub use components::*;
pub use rendering_manager::RenderingManager;

/// Registers the renderer's components, asset types and resources with an
/// engine, and installs its default pipeline; returns zero.
///
/// Registers no system: filling the frame and drawing it is the GPU module's
/// (`pill_master_renderer`), and a headless engine has neither. With
/// `module-abi` on, `#[pill_module]` also exports it as `pill_module_init`.
///
/// Components go in through [`register_components`]; the six asset types and
/// the two resources are declared so their per-type tables are re-pointed at
/// the artifact still mapped; the [`RenderFrame`] and [`RenderingManager`]
/// values are only inserted when missing - every step checks what is already
/// there, so a repeated call changes nothing.
///
/// It also installs the PBR pipeline ([`config::pbr_pipeline`]) and points the
/// manager at it, so a project that never declares a frame still draws one.
/// That is the *default*, not a decision: a project that wants a different
/// chain calls [`RenderingManager::set_pipeline`] after registering, and one
/// that wants the renderer's fallback chain clears the manager.
#[pill_module]
pub fn register(engine: &mut Engine) -> u32 {
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
    // Declare the resources this crate inserts, before inserting them. Once the
    // crate is a loaded module, a reload keeps the existing values and never
    // calls `insert_resource` again, so this declaration is what re-points
    // their drop functions at an image that is still mapped.
    engine.world_mut().register_resource::<RenderFrame>();
    engine.world_mut().register_resource::<RenderingManager>();
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
