//! GPU pass contracts adapted from Pill-Engine graphics/renderer.rs (af052d1).
//!
//! # Responsibilities
//!
//! - Describes buffers, shader stages, targets, and pipeline layouts.
//! - Defines the GPU context supplied to a rendering pass.
//!
//! # Design
//!
//! These types belong inside the GPU backend. The host communicates through
//! [`crate::PillRenderer`] instead, keeping wgpu types out of its renderer contract.
//! Pass contexts borrow device and target state only for a single draw call.

// Current crate
use crate::{RenderFrame, RenderViewport};

// =============================================================================
// Resource and Pipeline Descriptors
// =============================================================================

/// Allocation description for a GPU buffer.
#[derive(Clone, Copy)]
pub struct BufferDesc<'a> {
    /// Optional diagnostic label shown by backend validation and graphics tools.
    pub label: Option<&'a str>,
    /// Allocation size in bytes.
    pub byte_size: u64,
    /// Permitted GPU operations for the allocation.
    pub usage: wgpu::BufferUsages,
}

/// Borrowed shader source and the entry point for one pipeline stage.
#[derive(Clone, Copy, Debug)]
pub struct ShaderDesc<'a> {
    /// WGSL source text used to create a shader module.
    pub source: &'a str,
    /// Name of the vertex or fragment entry function.
    pub entry_func: &'a str,
}

/// Named two-dimensional render target allocation.
pub struct RendererTargetDesc {
    /// Diagnostic name for the target.
    pub name: String,
    /// Storage format and channel interpretation.
    pub format: wgpu::TextureFormat,
    /// Target width in physical pixels.
    pub width: u32,
    /// Target height in physical pixels.
    pub height: u32,
}

/// Shader and attachment requirements for a graphics pipeline.
pub struct PipelineV2Desc<'a> {
    /// Optional diagnostic label shown by backend validation and graphics tools.
    pub label: Option<&'a str>,
    /// Vertex shader stage.
    pub vertex: ShaderDesc<'a>,
    /// Fragment shader stage.
    pub fragment: ShaderDesc<'a>,
    /// Vertex streams, strides, and shader attribute locations.
    pub vertices: &'a [wgpu::VertexBufferLayout<'a>],
    /// Format of the pipeline's color attachment.
    pub color_format: wgpu::TextureFormat,
    /// Depth attachment format, or `None` for a pass without depth.
    pub depth_format: Option<wgpu::TextureFormat>,
}

/// Compiled graphics pipeline together with its bind-group layouts.
pub struct PipelineV2 {
    /// Backend pipeline object used when encoding draws.
    pub pipeline: wgpu::RenderPipeline,
    /// Layouts in the order expected by the shader's bind-group indices.
    pub bind_group_layouts: Vec<wgpu::BindGroupLayout>,
}

// =============================================================================
// Pass Execution
// =============================================================================

/// Borrowed backend state for one pass invocation.
pub struct GpuPassContext<'a> {
    /// Device used for any allocations needed during the draw.
    pub device: &'a wgpu::Device,
    /// Queue used for uploads and command submission.
    pub queue: &'a wgpu::Queue,
    /// Destination view for the pass's final color output.
    pub output: &'a wgpu::TextureView,
    /// Physical-pixel scene rectangle within the output target.
    pub viewport: RenderViewport,
}

/// Internal rendering operation consuming an extracted CPU frame.
pub trait Pass {
    /// Return a diagnostic name for this pass.
    fn label(&self) -> &str;

    /// Encode and submit the pass using borrowed GPU state and an owned-data packet.
    fn draw(&mut self, context: GpuPassContext<'_>, frame: &RenderFrame);
}
