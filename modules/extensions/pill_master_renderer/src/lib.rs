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

// The contract lives in `pill_renderer_api` and this renderer's data in
// `pill_master_renderer_data`; neither carries wgpu. Their modules are
// re-exported so this crate's own paths (`crate::api`, `crate::assets`,
// `crate::components`, `crate::config`) keep resolving.
pub use pill_master_renderer_data::{assets, components, config};
pub use pill_renderer_api::api;

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

/// The C ABI a host drives this renderer through when it loads it as a module.
#[cfg(feature = "module-abi")]
mod module_entry;

// External crates
use pill_engine::{pill_module, Engine};
pub use pill_master_renderer_data::components::*;
pub use pill_master_renderer_data::{
    AssetLoader, Material, MaterialBuilder, MaterialParameter, Mesh, MeshVertex, PassKind,
    PassTarget, RenderPass, RenderingPipeline, Shader, ShaderBuilder, ShaderParameterSlot,
    ShaderParameterType, ShaderTextureSlot, Texture, TextureType,
};
pub use pill_renderer_api::{
    FrameOutcome, HeadlessRenderer, PillRenderer, RenderCapabilities, RenderMetrics,
};

// Current crate
pub use error::RendererError;
pub use frame::{rendering_system, RenderFrame, RenderInstance, ResolvedPass};
pub use instance::Instance;
pub use renderer::Renderer;
pub use resources::RenderingManager;

/// The module's entry point: registers the post-update `rendering` system
/// that fills the frame, once, and returns zero.
///
/// Registers no data. The host registers `pill_renderer_api` - components,
/// asset types, resources, the default pipeline - before anything loads, in
/// every posture; registering it again from here would re-point the asset
/// types' tables at this image, the one that reloads most often.
///
/// The system is registered under whatever scope the caller opened: a host
/// loading this module scopes it to the module's own owner, so a reload clears
/// exactly this system and the next generation registers its own. With
/// `module-abi` on, `#[pill_module]` also exports this as `pill_module_init`.
#[pill_module]
pub fn register(engine: &mut Engine) -> u32 {
    if engine.is_system_enabled("rendering").is_none() {
        engine.register_post_update_system("rendering", rendering_system);
    }
    0
}

/// Start building a renderer backend on a window, for a host that links this
/// crate statically - the shipping posture, which has no module to load.
///
/// A future, because creating the device is asynchronous: a native frontend
/// blocks on it once, a web frontend awaits it. The loaded-module path reaches
/// the same renderer through the blocking `pill_renderer_attach` export
/// instead.
///
/// The future fails with a [`RendererError`] when surface, adapter or device
/// creation fails.
///
/// # Safety
///
/// `window` must name a live window that outlives the returned renderer.
pub unsafe fn attach(
    window: pill_renderer_api::RawWindowData,
    width: u32,
    height: u32,
) -> pill_renderer_api::AttachFuture {
    Box::pin(async move {
        // SAFETY: forwarded from this function's own contract, which covers
        // the future as much as the renderer it resolves to.
        let renderer = unsafe { Renderer::new_async(window, width, height) }.await?;
        Ok(Box::new(renderer) as Box<dyn PillRenderer>)
    })
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
