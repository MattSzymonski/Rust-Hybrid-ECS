//! The renderer contract: what the host hands any renderer and what every
//! renderer hands back, with no graphics stack behind it.
//!
//! # Responsibilities
//!
//! - Define the backend contract a frontend drives ([`PillRenderer`]), its
//!   results ([`FrameOutcome`], [`RenderCapabilities`], [`RenderMetrics`]) and
//!   the errors a renderer reports ([`RendererError`]).
//! - Define the frame a renderer draws ([`RenderFrame`]) and the vocabulary it
//!   is written in: pass kinds and targets, cull modes, material parameters.
//! - Define the contract's scene components - the camera the frame embeds and
//!   the viewport a frontend points a renderer at - and register them through
//!   [`register_contract_components`].
//! - Carry a window ([`RawWindowData`]), a surface set ([`SurfaceId`],
//!   [`SurfaceCameras`]) and the scene grid ([`SceneGrid`], the frame's cell
//!   size) as plain data.
//! - Carry debug drawing, selection outlines, picking and previews between the
//!   world and any renderer ([`SelectionOutlines`], [`PickRequests`],
//!   [`PickResults`], [`PreviewRequests`]).
//!
//! # Design
//!
//! This crate never depends on wgpu or winit, and holds nothing specific to
//! one renderer: a renderer comes as a data crate (assets, its own components,
//! pipelines, shaders) plus a GPU module, and both build on this contract. The
//! host links this crate in every posture, so a change here needs a host
//! restart; the types stay small and data-shaped.

/// The renderer contract a frontend drives, plus a headless stub.
pub mod api;

/// The contract's scene components - camera and viewport - and
/// [`register_contract_components`].
pub mod components;

/// Renderer failures, the `Result` alias and the context helper.
pub mod error;

/// The frame a renderer draws: instances, resolved passes, camera and clock.
pub mod frame;

/// The scene grid of the view-bound surfaces: its switch, spacing and
/// resolution into a frame.
pub mod grid;

/// A window's platform handles as plain data, for attaching a renderer to it.
pub mod raw_window;

/// Several surfaces per renderer: identities, presentation, camera bindings.
pub mod surface;

/// Selection outlines and picking.
pub mod picking;

/// Asset previews: off-screen renders the editor asks a renderer for.
pub mod preview;

// Current crate
pub use api::{
    AttachFuture, FrameOutcome, HeadlessRenderer, PillRenderer, RenderCapabilities, RenderMetrics,
};
pub use components::*;
pub use error::RendererError;
pub use frame::{
    AssetKey, ChainSource, CullMode, MaterialParameter, PassKind, PassSettings, PassTarget,
    RenderFrame, RenderInstance, ResolvedPass,
};
pub use grid::{editor_view_surfaces, frame_grid, SceneGrid};
pub use picking::{
    decode_pick_value, PickHit, PickRequest, PickRequests, PickResult, PickResults,
    SelectionOutlines, PICK_HANDLE_BIT,
};
pub use preview::{
    ensure_preview_resources, PreviewAsset, PreviewImage, PreviewRequest, PreviewRequests,
    PreviewResults,
};
pub use raw_window::{RawWindowData, RawWindowKind};
pub use surface::{
    resolve_camera_binding, CameraBinding, PresentPreference, SurfaceCameras, SurfaceId,
    SurfaceOptions, SurfaceView,
};

#[cfg(test)]
mod tests {
    use super::*;
    use pill_engine::Resource;

    /// Every resource the contract defines pins its shared name exactly.
    ///
    /// The names are identities, not paths: the host and a renderer module
    /// resolve the same resource by them across the artifact boundary, so a
    /// "corrected" path would silently register a second resource and leave
    /// the first stale. Some of them name a module this crate does not contain,
    /// which is deliberate - they have always been spelled that way.
    #[test]
    fn every_contract_resource_pins_its_shared_name() {
        assert_eq!(
            RenderFrame::shared_name(),
            Some("pill_master_renderer::frame::RenderFrame")
        );
        assert_eq!(
            SurfaceCameras::shared_name(),
            Some("pill_renderer_api::surface::SurfaceCameras")
        );
        assert_eq!(
            SelectionOutlines::shared_name(),
            Some("pill_renderer_api::picking::SelectionOutlines")
        );
        assert_eq!(
            PickRequests::shared_name(),
            Some("pill_renderer_api::picking::PickRequests")
        );
        assert_eq!(
            PickResults::shared_name(),
            Some("pill_renderer_api::picking::PickResults")
        );
        assert_eq!(
            PreviewRequests::shared_name(),
            Some("pill_renderer_api::preview::PreviewRequests")
        );
        assert_eq!(
            PreviewResults::shared_name(),
            Some("pill_renderer_api::preview::PreviewResults")
        );
    }
}
