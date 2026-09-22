//! Backend-neutral communication between pill_host and the renderer.
//!
//! # Responsibilities
//!
//! - Defines frame outcomes, capabilities, and CPU-side rendering metrics.
//! - Provides the renderer interface used by frontends and a headless implementation.
//!
//! # Design
//!
//! The host supplies owned ECS packets and physical-pixel viewport changes.
//! Window handles, GPU objects, and executor choices stay in backend construction,
//! so a host can store a boxed [`PillRenderer`] without exposing those dependencies.

// Current crate
use crate::{RenderFrame, RenderViewport, RendererError};

// =============================================================================
// Frame Results and Diagnostics
// =============================================================================

/// Result of a frame attempt that did not encounter a fatal error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOutcome {
    /// The backend submitted the frame for presentation.
    Presented,
    /// No frame was presented, for example while minimized or running headless.
    Skipped,
}

/// Backend limits available to the host; defaults describe no GPU capability.
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderCapabilities {
    /// Maximum supported two-dimensional texture extent along either axis.
    pub max_texture_size: u32,
    /// Maximum size of a single GPU buffer, in bytes.
    pub max_buffer_bytes: u64,
    /// Whether the backend supports the linear HDR intermediate used by PBR.
    pub hdr: bool,
}

/// CPU timings and submission counts from the latest rendered frame.
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderMetrics {
    /// CPU time spent preparing assets, uniforms, and instance uploads.
    pub prepare_micros: u64,
    /// CPU time spent encoding and submitting commands; not GPU execution time.
    pub submit_micros: u64,
    /// Encoded draw calls, including the background and tonemap passes.
    pub draw_calls: u32,
    /// Bytes of instance data uploaded for this frame.
    pub instance_bytes: u64,
}

// =============================================================================
// Host Contract
// =============================================================================

/// Host-owned rendering backend, independent of the ECS storage lifetime.
pub trait PillRenderer {
    /// Describe supported limits; headless or minimal backends may use defaults.
    fn capabilities(&self) -> RenderCapabilities {
        RenderCapabilities::default()
    }

    /// Return the latest backend counters without synchronizing with the GPU.
    fn metrics(&self) -> RenderMetrics {
        RenderMetrics::default()
    }

    /// Apply a physical window extent; zero dimensions suspend presentation.
    fn resize(&mut self, width: u32, height: u32);

    /// Select a physical-pixel scene rectangle, or `None` for the full surface.
    fn set_viewport(&mut self, viewport: Option<RenderViewport>);

    /// Submit an extracted scene packet, reporting fatal backend failures as errors.
    fn render(&mut self, frame: &RenderFrame) -> Result<FrameOutcome, RendererError>;

    /// Mark cached uploads stale so the backend refreshes them on a later frame.
    fn invalidate_assets(&mut self);
}

// =============================================================================
// Headless Backend
// =============================================================================

/// No-op backend for hosts that need ECS extraction without a GPU or window.
#[derive(Default)]
pub struct HeadlessRenderer;
impl PillRenderer for HeadlessRenderer {
    fn resize(&mut self, _: u32, _: u32) {}
    fn set_viewport(&mut self, _: Option<RenderViewport>) {}
    fn render(&mut self, _: &RenderFrame) -> Result<FrameOutcome, RendererError> {
        Ok(FrameOutcome::Skipped)
    }
    fn invalidate_assets(&mut self) {}
}
