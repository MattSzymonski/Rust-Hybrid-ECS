//! Host-facing renderer contract retained by `pill_host`.
//!
//! # Responsibilities
//!
//! - Define [`PillRenderer`], the trait a frontend drives once per frame:
//!   resize, viewport, frame submission, and asset invalidation.
//! - Report each frame's result and cost through [`FrameOutcome`],
//!   [`RenderCapabilities`], and [`RenderMetrics`], so frontends can display
//!   statistics without knowing which backend produced them.
//! - Provide [`HeadlessRenderer`], the stub that keeps builds and tests
//!   without a GPU surface on the same call path as the wgpu backend.
//!
//! # Design
//!
//! The host stores its renderer as a `Box<dyn PillRenderer>`, so this module
//! keeps to the values that cross that boundary - [`RenderFrame`],
//! [`AssetManager`], [`RenderViewport`], and [`RendererError`] - and stays free
//! of wgpu types. A windowed build implements the trait on its wgpu `Renderer`;
//! a headless build holds [`HeadlessRenderer`] instead, and the frame loop
//! cannot tell the difference.

// External crates
use pill_engine::AssetManager;

// Current crate
use crate::{RenderFrame, RenderViewport, RendererError};

/// What one [`PillRenderer::render`] call did with the frame it was given.
///
/// A skip is not a failure: the caller keeps running and simply presents
/// nothing this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOutcome {
    /// The frame was drawn and submitted to the surface.
    Presented,
    /// Nothing was drawn: the surface is minimized, the frame carries no
    /// camera, or the renderer is a stub with no GPU behind it.
    Skipped,
}

/// What the renderer can do: the limits of its device and the scene data it
/// draws.
///
/// Exposes what the device can accept, so callers that size transient
/// resources - render targets, uploads - do not have to assume desktop
/// maxima. A renderer with no GPU behind it reports the zero defaults.
///
/// Not `Copy`: `consumed_components` is an owned list. It is owned rather than
/// `&'static` on purpose - a renderer loaded as a module builds it inside its
/// own image, and a static slice would point into that image after a reload
/// unmaps it.
#[derive(Debug, Clone, Default)]
pub struct RenderCapabilities {
    /// Largest supported 2D texture dimension, in texels.
    pub max_texture_size: u32,
    /// Largest single GPU buffer the device accepts, in bytes.
    pub max_buffer_bytes: u64,
    /// Whether the surface presents high-dynamic-range output. No backend
    /// reports `true` yet.
    pub hdr: bool,
    /// Shared names of the components this renderer reads to build a frame.
    ///
    /// Lets the host warn when a project uses render data the active renderer
    /// ignores (for example a component another renderer's data crate
    /// declares). Empty for a renderer that reads nothing, like the headless
    /// stub.
    pub consumed_components: Vec<String>,
}

/// Frame statistics from the most recent [`PillRenderer::render`] call.
///
/// The windowed renderer fills these in as it works, and the host reads them
/// to report frame diagnostics; a renderer that has not drawn yet leaves
/// every field at zero.
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderMetrics {
    /// CPU time spent preparing the frame: syncing assets, rebuilding the
    /// chain's objects if they moved, and building and sorting the draw queue.
    pub prepare_micros: u64,
    /// CPU time spent submitting the recorded work to the GPU.
    pub submit_micros: u64,
    /// Passes of the chain that were planned with at least one draw.
    pub draw_calls: u32,
    /// Passes the frame ran, whether or not they drew anything.
    pub passes: u32,
    /// Instance bytes the frame's draw queue carried.
    pub instance_bytes: u64,
}

/// The renderer contract a frontend drives once per frame.
///
/// Boxing it as `Box<dyn PillRenderer>` is what lets a frontend swap the wgpu
/// backend for [`HeadlessRenderer`] without touching its frame loop.
pub trait PillRenderer {
    /// Reports the limits of the device the renderer draws on.
    ///
    /// The default reports zeroed limits, which is all a renderer with no GPU
    /// behind it has to say.
    fn capabilities(&self) -> RenderCapabilities {
        RenderCapabilities::default()
    }

    /// Returns the statistics of the most recent frame.
    ///
    /// The counts are only meaningful after a successful [`Self::render`];
    /// before the first frame they stay at zero.
    fn metrics(&self) -> RenderMetrics {
        RenderMetrics::default()
    }

    /// Tells the renderer that its surface now measures `width` by `height`
    /// physical pixels.
    ///
    /// A zero dimension means the surface is minimized; frames skip until a
    /// usable size arrives.
    fn resize(&mut self, width: u32, height: u32);

    /// Restricts drawing to a sub-region of the surface.
    ///
    /// `None` restores full-surface rendering. Embedded frontends use this to
    /// keep drawing out of the UI regions they overlay.
    fn set_viewport(&mut self, viewport: Option<RenderViewport>);

    /// Draws one resolved frame from `assets` and presents it.
    ///
    /// Returns [`FrameOutcome::Skipped`] when there is nothing to present - a
    /// minimized surface or a frame without a camera - which is a normal
    /// outcome, not an error.
    ///
    /// The store is read rather than copied into the frame: the renderer diffs
    /// it against what it last uploaded, so only the assets that moved are
    /// rebuilt, and a frame never carries a duplicate of the project's asset
    /// data. The borrow lasts the call, so nothing can change under it.
    ///
    /// # Errors
    ///
    /// Fails with [`RendererError`] when the GPU work itself fails: a lost or
    /// out-of-memory surface, a camera handle with no camera behind it, or a
    /// command buffer wgpu refuses at submission. An asset that will not build
    /// is not an error - it is reported per pass and skipped.
    fn render(
        &mut self,
        frame: &RenderFrame,
        assets: &AssetManager,
    ) -> Result<FrameOutcome, RendererError>;

    /// Forces the next frame to rebuild every GPU object from the asset store.
    ///
    /// The renderer normally rebuilds only what changed, tracked by content
    /// versions; this clears that bookkeeping for callers that know the asset
    /// set was replaced wholesale and the diff can no longer be trusted.
    fn invalidate_assets(&mut self);
}

/// Stub renderer for frontends that run without a GPU surface.
///
/// Every call is accepted and forgotten: resize, viewport, and asset
/// invalidation do nothing, and [`PillRenderer::render`] reports
/// [`FrameOutcome::Skipped`] so a headless frame loop runs unchanged.
#[derive(Default)]
pub struct HeadlessRenderer;

impl PillRenderer for HeadlessRenderer {
    fn resize(&mut self, _: u32, _: u32) {}
    fn set_viewport(&mut self, _: Option<RenderViewport>) {}
    fn render(&mut self, _: &RenderFrame, _: &AssetManager) -> Result<FrameOutcome, RendererError> {
        Ok(FrameOutcome::Skipped)
    }
    fn invalidate_assets(&mut self) {}
}
