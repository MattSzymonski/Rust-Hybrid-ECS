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

/// Bind group indices and the instance batch size every pipeline is built around.
pub mod config;

/// The mesh drawer: batches queued entities and records the instanced draws.
pub mod drawers;

/// Renderer failures, the `Result` alias, and the wgpu validation capture.
pub mod error;

/// Render instances, resolved passes, and the system that builds a frame from the world.
pub mod frame;

/// The per-instance transform the drawer uploads for the vertex shader.
pub mod instance;

/// The query sets and readbacks behind GPU profiling measurements.
pub mod profiler;

/// Queue items and the packed key fields that order a frame's draws.
pub mod render_queue;

/// The wgpu renderer: device and surface lifecycle, pipelines and submission.
pub mod renderer;

/// The GPU resource handles the renderer's slot maps are keyed by.
mod resource_handles;

/// One module per cached GPU resource, re-exported flat for the renderer.
pub mod resources;

/// The window surface, its swapchain, and the lifecycle that keeps them live.
mod surface;

// External crates
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
    if engine.is_system_enabled("rendering").is_none() {
        engine.begin_module_registration(pill_engine::SystemOwner::ENGINE);
        engine.register_post_update_system("rendering", rendering_system);
        engine.end_module_registration();
    }
    0
}
