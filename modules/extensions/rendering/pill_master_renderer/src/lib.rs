//! The wgpu renderer: the GPU machinery that turns the world's renderer data
//! into frames.
//!
//! # Responsibilities
//!
//! - Own the GPU half: device and surface lifecycle ([`Renderer`]), the
//!   [`resources`] caches, the [`drawers`], and the [`render_queue`] keys that
//!   order a frame's draws.
//! - Fill each frame from the world ([`frame`], the `rendering` system).
//! - Install the renderer on an engine through [`register`]: the data from
//!   `pill_renderer_api`, plus the system.
//!
//! # Design
//!
//! The data a project draws with - assets, components, the default pipelines
//! and their WGSL, the frame, the backend trait - lives in `pill_renderer_api`,
//! which carries no wgpu and is re-exported here. This crate is the only one
//! that links wgpu, which is what will let it be loaded and reloaded as a
//! module while the host and projects link only the data.
//!
//! Transferred from the original `pill_renderer` crate. The GPU resource,
//! shader, material, mesh, camera, drawer, and surface code remains in the
//! original module structure.
//!
//! The host still links this crate directly under its `rendering` feature;
//! making it a reloadable module is the remaining work of
//! `local/docs/plans/renderer_hot_reload.md`.

// The data half lives in `pill_renderer_api`, which carries no wgpu; its
// modules are re-exported so this crate's own paths (`crate::assets`,
// `crate::components`, `crate::config`) keep resolving.
pub use pill_renderer_api::{api, assets, components, config};

/// The mesh drawer: batches queued entities and records the instanced draws.
pub mod drawers;

/// Renderer failures, re-exported, and the wgpu validation capture.
pub mod error;

/// The system that turns the world into the frame the renderer draws.
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
pub use pill_renderer_api::components::*;
pub use pill_renderer_api::{
    register_components, AssetLoader, FrameOutcome, HeadlessRenderer, Material, MaterialBuilder,
    MaterialParameter, Mesh, MeshVertex, PassKind, PassTarget, PillRenderer, RenderCapabilities,
    RenderMetrics, RenderPass, RenderingPipeline, Shader, ShaderBuilder, ShaderParameterSlot,
    ShaderParameterType, ShaderTextureSlot, Texture, TextureType,
};
#[cfg(feature = "shader-hot-reload")]
pub use pill_renderer_api::{shader_reload, ShaderReloadReport, ShaderReloader};

// Current crate
pub use error::RendererError;
pub use frame::{rendering_system, RenderFrame, RenderInstance, ResolvedPass};
pub use instance::Instance;
pub use renderer::Renderer;
pub use resources::RenderingManager;
pub use surface::RendererWindow;

/// Registers the renderer's data and its `rendering` system with an engine,
/// and returns zero.
///
/// The data - components, asset types, resources and the default pipeline -
/// is [`pill_renderer_api::register`]'s; this adds the post-update `rendering`
/// system that fills the frame, once. Every step checks what is already there,
/// so a repeated call changes nothing.
pub fn register(engine: &mut pill_engine::Engine) -> u32 {
    pill_renderer_api::register(engine);
    if engine.is_system_enabled("rendering").is_none() {
        engine.begin_module_registration(pill_engine::SystemOwner::ENGINE);
        engine.register_post_update_system("rendering", rendering_system);
        engine.end_module_registration();
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use pill_engine::Engine;

    /// The renderer's registration adds exactly one `rendering` system on top
    /// of the data, however often it runs.
    #[test]
    fn registering_the_renderer_adds_the_rendering_system_once() {
        let mut engine = Engine::new();

        assert_eq!(register(&mut engine), 0);
        assert_eq!(register(&mut engine), 0);

        assert!(engine.is_system_enabled("rendering").is_some());
    }
}
