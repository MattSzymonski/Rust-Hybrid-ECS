//! The renderer contract: what the host hands *any* renderer, and what every
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
//! - Carry a window across to a renderer as plain data ([`RawWindowData`]).
//! - Let one renderer draw into several surfaces, each through its own camera
//!   ([`SurfaceId`], [`SurfaceCameras`]).
//! - Carry the scene grid an editor draws in its view-bound surfaces
//!   ([`SceneGrid`], [`FrameGrid`]).
//! - Carry debug drawing, selection outlines and picking between the world and
//!   any renderer ([`SelectionOutlines`], [`PickRequests`], [`PickResults`]).
//!
//! # Design
//!
//! This crate never depends on wgpu or winit, and holds nothing specific to
//! one renderer. A renderer comes as two crates: its data (assets, its own
//! components, pipelines, shaders - `pill_master_renderer_data` for the master
//! renderer) and its GPU module. Both build on this contract, and so does the
//! host, which drives any renderer through [`PillRenderer`] without naming its
//! types.
//!
//! The host links this crate, so a change here needs a host restart; it
//! therefore stays small and data-shaped.

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

// Current crate
pub use api::{
    AttachFuture, FrameOutcome, HeadlessRenderer, PillRenderer, RenderCapabilities, RenderMetrics,
};
pub use components::*;
pub use error::RendererError;
pub use frame::{
    CullMode, MaterialParameter, PassKind, PassTarget, RenderFrame, RenderInstance, ResolvedPass,
};
pub use grid::{frame_grid, FrameGrid, SceneGrid};
pub use picking::{
    decode_pick_value, PickHit, PickRequest, PickRequests, PickResult, PickResults,
    SelectionOutlines, PICK_HANDLE_BIT,
};
pub use raw_window::{RawWindowData, RawWindowKind};
pub use surface::{
    resolve_camera_binding, CameraBinding, PresentPreference, SurfaceCameras, SurfaceId,
    SurfaceOptions, SurfaceView,
};
