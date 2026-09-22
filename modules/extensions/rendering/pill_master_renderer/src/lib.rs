//! ECS-driven PBR rendering, with GPU lifetime owned by the host.
//!
//! # Responsibilities
//!
//! - Registers shared scene components, scene settings, and frame extraction.
//! - Exposes an owned frame packet and the backend-neutral host renderer contract.
//! - Gates GPU rendering and native asset cooking behind separate features.
//!
//! # Design
//!
//! The project describes a scene through the existing ECS. [`register`] installs
//! one engine-owned post-update system, which copies that scene after deferred
//! commands have been applied. The host then submits the packet to [`PillRenderer`].
//! GPU resources never become components or persistable world resources.
//!
//! The contract layer works without the `gpu` feature. Byte-based asset loading
//! and asynchronous GPU initialization leave room for another platform frontend;
//! the filesystem cooker remains a native tool behind `asset-cooking`.

// =============================================================================
// Scene Contracts and Backend Exports
// =============================================================================

pub mod assets;
pub mod component;
pub mod frame;
pub use component::*;
pub use frame::{rendering_system, RenderFrame, RenderInstance};
pub mod error;
#[cfg(feature = "gpu")]
pub mod pbr;
#[cfg(feature = "gpu")]
pub mod renderer;
pub use error::RendererError;
#[cfg(feature = "gpu")]
pub use renderer::{Renderer, RendererWindow};

// =============================================================================
// Registration
// =============================================================================

/// Install scene contracts and one engine-owned post-update extraction system.
///
/// Repeated calls preserve existing resources and do not add another rendering
/// system. Engine ownership keeps extraction alive when project systems reload;
/// the post-update stage also runs while gameplay systems are paused.
/// Returns zero, matching the extension registration status convention.
pub fn register(engine: &mut pill_engine::Engine) -> u32 {
    // Step 1: register identities and persistence without replacing scene settings.
    register_components(engine.world_mut());
    engine
        .world_mut()
        .register_persistable_resource::<RenderSettings>();
    if engine.world().get_resource::<RenderSettings>().is_none() {
        engine
            .world_mut()
            .insert_resource(RenderSettings::default());
    }
    if engine
        .world()
        .get_resource::<assets::RenderAssetRequests>()
        .is_none()
    {
        engine
            .world_mut()
            .insert_resource(assets::RenderAssetRequests::default());
    }
    if engine.world().get_resource::<RenderFrame>().is_none() {
        engine.world_mut().insert_resource(RenderFrame::default());
    }
    // Step 2: install extraction once, outside the reloadable project owner.
    if engine.is_system_enabled("rendering").is_none() {
        engine.begin_module_registration(pill_engine::SystemOwner::ENGINE);
        engine.register_post_update_system("rendering", rendering_system);
        engine.end_module_registration();
    }
    0
}

// =============================================================================
// Feature-Gated Implementations
// =============================================================================

#[cfg(feature = "asset-cooking")]
pub mod pill_assets;

#[cfg(feature = "gpu")]
mod gpu_assets;

pub mod api;
pub use api::{FrameOutcome, HeadlessRenderer, PillRenderer};

#[cfg(feature = "gpu")]
pub mod graphics;
pub mod render_queue;

#[cfg(test)]
mod validation;
