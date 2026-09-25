//! Pill renderer transferred from the original `pill_renderer` crate.
//!
//! The GPU resource, shader, material, mesh, camera, drawer, and surface code
//! remains in the original module structure. The added asset and frame modules
//! adapt that backend to this repository's `AssetManager` and ECS scheduler.

pub mod api;
pub mod assets;
pub mod component;
pub mod config;
pub mod drawers;
pub mod error;
pub mod frame;
pub mod instance;
pub mod render_queue;
pub mod renderer;
pub mod resources;
mod slot_map;
#[cfg(feature = "debug_ui")]
mod timer;

pub use api::{FrameOutcome, HeadlessRenderer, PillRenderer, RenderCapabilities, RenderMetrics};
pub use assets::{
    Material, MaterialBuilder, MaterialParameter, Mesh, MeshVertex, PassKind, PassTarget,
    RenderPass, RenderingPipeline, Shader, ShaderParameterSlot, ShaderParameterType,
    ShaderTextureSlot, Texture, TextureType,
};
pub use component::*;
pub use error::RendererError;
pub use frame::{rendering_system, AssetSnapshot, RenderFrame, RenderInstance, ResolvedPass};
pub use instance::Instance;
pub use pill_engine::AssetLoader;
pub use renderer::{Renderer, RendererWindow};
pub use resources::RenderingManager;

pub fn register(engine: &mut pill_engine::Engine) -> u32 {
    register_components(engine.world_mut());
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
