//! Minimal 2D sprite renderer (wgpu-backed), gated behind the `rendering` feature.
//!
//! # Responsibilities
//!
//! - Defines [`Position`] and [`Sprite`] components describing where and how
//!   to draw an entity as a colored rectangle.
//! - Provides [`SpriteRenderer`], the low-level sprite pipeline used by the
//!   engine-owned [`Renderer`](crate::Renderer).
//!
//! # Design
//!
//! This module intentionally stays tiny: it draws colored rectangles, not a
//! general sprite/texture pipeline. [`crate::Renderer`] owns the normal window
//! surface/device/queue lifecycle. Advanced integrations such as the editor
//! may still drive `SpriteRenderer` directly when they must attach to a surface
//! whose lifetime is owned by another UI framework.

// External crates
use trait_type_map::impl_trait_accessible;
use wgpu::util::DeviceExt;

// Current crate
use crate::component::Component;
use crate::query::Query;
use crate::world::World;

// =============================================================================
// Components
// =============================================================================

/// World-space position of an entity's top-left draw origin, in pixels.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Position {
    pub x: f32,
    pub y: f32,
}
impl Component for Position {}
impl_trait_accessible!(dyn Component; Position);

/// Plain RGBA color, backend-agnostic (0.0-1.0 per channel).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const WHITE: Color = Color {
        r: 1.0,
        g: 1.0,
        b: 1.0,
        a: 1.0,
    };

    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }
}

impl Default for Color {
    fn default() -> Self {
        Self::WHITE
    }
}

/// Axis-aligned colored rectangle drawn at an entity's [`Position`].
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Sprite {
    pub width: f32,
    pub height: f32,
    pub color: Color,
}
impl Component for Sprite {}
impl_trait_accessible!(dyn Component; Sprite);

impl Default for Sprite {
    fn default() -> Self {
        Self {
            width: 16.0,
            height: 16.0,
            color: Color::WHITE,
        }
    }
}

// =============================================================================
// GPU instance data
// =============================================================================

/// Per-instance data uploaded to the GPU for one sprite quad.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct SpriteInstance {
    /// Top-left position in pixels.
    position: [f32; 2],
    /// Width/height in pixels.
    size: [f32; 2],
    /// RGBA color.
    color: [f32; 4],
}

/// Uniform holding the viewport size, used to convert pixel coordinates to
/// normalized device coordinates in the vertex shader.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ViewportUniform {
    width: f32,
    height: f32,
    _padding: [f32; 2],
}

const SHADER_SOURCE: &str = r#"
struct Viewport {
    size: vec2<f32>,
};
@group(0) @binding(0) var<uniform> viewport: Viewport;

struct InstanceInput {
    @location(0) position: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

// Unit quad corners, expanded per-instance by `size` and offset by `position`.
var<private> CORNERS: array<vec2<f32>, 6> = array<vec2<f32>, 6>(
    vec2<f32>(0.0, 0.0),
    vec2<f32>(1.0, 0.0),
    vec2<f32>(0.0, 1.0),
    vec2<f32>(0.0, 1.0),
    vec2<f32>(1.0, 0.0),
    vec2<f32>(1.0, 1.0),
);

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32, instance: InstanceInput) -> VertexOutput {
    let corner = CORNERS[vertex_index];
    let pixel_position = instance.position + corner * instance.size;
    // Pixel space (origin top-left, +y down) -> NDC (origin center, +y up).
    let ndc = vec2<f32>(
        (pixel_position.x / viewport.size.x) * 2.0 - 1.0,
        1.0 - (pixel_position.y / viewport.size.y) * 2.0,
    );

    var out: VertexOutput;
    out.clip_position = vec4<f32>(ndc, 0.0, 1.0);
    out.color = instance.color;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

// =============================================================================
// SpriteRenderer
// =============================================================================

/// Draws every `(Position, Sprite)` entity as an instanced colored quad.
///
/// Owns a render pipeline and reusable GPU buffers; does not own a window,
/// surface, device, or queue - callers create those and pass them in.
pub struct SpriteRenderer {
    pipeline: wgpu::RenderPipeline,
    viewport_buffer: wgpu::Buffer,
    viewport_bind_group: wgpu::BindGroup,
    instance_buffer: wgpu::Buffer,
    instance_capacity: usize,
}

impl SpriteRenderer {
    /// Initial instance-buffer capacity, in sprites. Grown on demand.
    const INITIAL_CAPACITY: usize = 256;

    /// Build the render pipeline for the given device and target format.
    ///
    /// `target_format` must match the surface/texture format the renderer
    /// will draw into.
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("sprite shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SOURCE.into()),
        });

        let viewport_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("sprite viewport uniform"),
            size: std::mem::size_of::<ViewportUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sprite viewport bind group layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let viewport_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sprite viewport bind group"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: viewport_buffer.as_entire_binding(),
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sprite pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let instance_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<SpriteInstance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &wgpu::vertex_attr_array![
                0 => Float32x2, // position
                1 => Float32x2, // size
                2 => Float32x4, // color
            ],
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("sprite pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[instance_layout],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("sprite instance buffer"),
            size: (Self::INITIAL_CAPACITY * std::mem::size_of::<SpriteInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            viewport_buffer,
            viewport_bind_group,
            instance_buffer,
            instance_capacity: Self::INITIAL_CAPACITY,
        }
    }

    /// Draw every `(Position, Sprite)` entity in `world` into `view`.
    ///
    /// `viewport_width`/`viewport_height` are the render target's size in
    /// pixels, used to map pixel coordinates to clip space.
    pub fn render(
        &mut self,
        world: &mut World,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &wgpu::TextureView,
        viewport_width: u32,
        viewport_height: u32,
    ) {
        let instances: Vec<SpriteInstance> = {
            let mut query = Query::<(&Position, &Sprite)>::new(world);
            query
                .iter_mut()
                .map(|(position, sprite)| SpriteInstance {
                    position: [position.x, position.y],
                    size: [sprite.width, sprite.height],
                    color: [
                        sprite.color.r,
                        sprite.color.g,
                        sprite.color.b,
                        sprite.color.a,
                    ],
                })
                .collect()
        };

        queue.write_buffer(
            &self.viewport_buffer,
            0,
            bytemuck::bytes_of(&ViewportUniform {
                width: viewport_width.max(1) as f32,
                height: viewport_height.max(1) as f32,
                _padding: [0.0, 0.0],
            }),
        );

        if !instances.is_empty() {
            if instances.len() > self.instance_capacity {
                self.instance_capacity = instances.len().next_power_of_two();
                self.instance_buffer =
                    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("sprite instance buffer"),
                        contents: bytemuck::cast_slice(&instances),
                        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    });
            } else {
                queue.write_buffer(&self.instance_buffer, 0, bytemuck::cast_slice(&instances));
            }
        }

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("sprite render encoder"),
        });

        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("sprite render pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            if !instances.is_empty() {
                render_pass.set_pipeline(&self.pipeline);
                render_pass.set_bind_group(0, &self.viewport_bind_group, &[]);
                render_pass.set_vertex_buffer(0, self.instance_buffer.slice(..));
                render_pass.draw(0..6, 0..instances.len() as u32);
            }
        }

        queue.submit(Some(encoder.finish()));
    }
}
