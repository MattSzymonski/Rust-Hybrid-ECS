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

/// A window's platform handles as plain data, for attaching a renderer to it.
pub mod raw_window;

// Current crate
pub use api::{FrameOutcome, HeadlessRenderer, PillRenderer, RenderCapabilities, RenderMetrics};
pub use components::*;
pub use error::RendererError;
pub use frame::{
    CullMode, MaterialParameter, PassKind, PassTarget, RenderFrame, RenderInstance, ResolvedPass,
};
pub use raw_window::{RawWindowData, RawWindowKind};
