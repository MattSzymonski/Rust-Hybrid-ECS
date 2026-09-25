//! Host-facing renderer contract retained by `pill_host`.

use crate::{RenderFrame, RenderViewport, RendererError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOutcome {
    Presented,
    Skipped,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RenderCapabilities {
    pub max_texture_size: u32,
    pub max_buffer_bytes: u64,
    pub hdr: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RenderMetrics {
    pub prepare_micros: u64,
    pub submit_micros: u64,
    /// Passes of the chain that recorded at least one draw.
    pub draw_calls: u32,
    /// Passes the frame ran, whether or not they drew anything.
    pub passes: u32,
    pub instance_bytes: u64,
}

pub trait PillRenderer {
    fn capabilities(&self) -> RenderCapabilities {
        RenderCapabilities::default()
    }
    fn metrics(&self) -> RenderMetrics {
        RenderMetrics::default()
    }
    fn resize(&mut self, width: u32, height: u32);
    fn set_viewport(&mut self, viewport: Option<RenderViewport>);
    fn render(&mut self, frame: &RenderFrame) -> Result<FrameOutcome, RendererError>;
    fn invalidate_assets(&mut self);
}

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
