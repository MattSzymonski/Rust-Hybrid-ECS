//! GPU half of the sprite renderer: the wgpu pipeline and its buffers.
//!
//! # Responsibilities
//!
//! - Owns the WGSL sprite shader and the render pipeline built from it.
//! - Owns the reusable viewport-uniform and per-instance GPU buffers.
//! - Draws every `(Position, Sprite)` entity the world reports through
//!   [`World::sprite_instances`](pill_engine::world::World::sprite_instances).
//!
//! # Design
//!
//! The GPU half of the split whose data half is [`pill_engine::render`]: the
//! components, their field layouts, the viewport types and the instance
//! record. That split is what lets a module or a hot patch link the
//! renderer's DATA contract without linking wgpu - `pill_engine` is an rlib
//! compiled into every loaded DLL, so anything this crate touches would
//! otherwise land in all of them.
//!
//! The GPU record below is deliberately private and separate from
//! `pill_engine::render::SpriteInstance`, so `bytemuck` stays on this side of
//! the split too.

// External crates
use pill_engine::render::{RenderViewport, SpriteInstance, VirtualResolution};
use pill_engine::world::World;
use wgpu::util::DeviceExt;

// =============================================================================
// Constants
// =============================================================================


/// WGSL source for the sprite render pipeline.
///
/// Declares the `Viewport` uniform, the per-instance vertex inputs, and both
/// the vertex and fragment entry points for instanced quad rendering.
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

/// Viewport background, as sRGB channel values in the 0.0-1.0 range.
///
/// A dark desaturated blue-grey (roughly `#23282F`), chosen so sprites read
/// clearly against it without the background competing for attention.
const VIEWPORT_BACKGROUND_SRGB: [f64; 3] = [35.0 / 255.0, 40.0 / 255.0, 47.0 / 255.0];

/// Convert one sRGB channel to linear.
///
/// `wgpu` interprets a clear value as LINEAR when the attachment format is
/// sRGB, and writes it through unchanged otherwise. Picking a colour by eye in
/// sRGB and skipping this conversion is the classic way to end up with a
/// background that looks right on one adapter and nearly black on another, so
/// the value above is stated once in sRGB and converted per format.
fn srgb_channel_to_linear(channel: f64) -> f64 {
    if channel <= 0.04045 {
        channel / 12.92
    } else {
        ((channel + 0.055) / 1.055).powf(2.4)
    }
}

/// The clear colour for a target of `format`, from [`VIEWPORT_BACKGROUND_SRGB`].
fn viewport_background(format: wgpu::TextureFormat) -> wgpu::Color {
    let [red, green, blue] = VIEWPORT_BACKGROUND_SRGB;
    let convert = |channel: f64| {
        if format.is_srgb() {
            srgb_channel_to_linear(channel)
        } else {
            channel
        }
    };
    wgpu::Color {
        r: convert(red),
        g: convert(green),
        b: convert(blue),
        // Opaque: the viewport is a solid background now, not a transparent
        // hole a compositor fills in.
        a: 1.0,
    }
}

// =============================================================================
// GPU records
// =============================================================================

/// Per-instance data as the vertex shader reads it.
///
/// A private mirror of [`SpriteInstance`] that carries the `bytemuck` derives
/// needed to upload it as raw bytes. The engine-side type stays free of
/// `bytemuck` so the data half of the renderer has no GPU dependency at all;
/// the two must therefore be laid out identically, which the assertion below
/// checks at compile time rather than trusting a comment. If they ever drift,
/// sprites would render as garbage with no error reported anywhere.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct GpuSpriteInstance {
    /// Top-left position in pixels.
    position: [f32; 2],
    /// Width/height in pixels.
    size: [f32; 2],
    /// RGBA color.
    color: [f32; 4],
}

const _: () = {
    assert!(
        std::mem::size_of::<GpuSpriteInstance>() == std::mem::size_of::<SpriteInstance>(),
        "the GPU sprite record must be the same size as `SpriteInstance`"
    );
    assert!(
        std::mem::align_of::<GpuSpriteInstance>() == std::mem::align_of::<SpriteInstance>(),
        "the GPU sprite record must have the same alignment as `SpriteInstance`"
    );
    assert!(
        std::mem::offset_of!(GpuSpriteInstance, position)
            == std::mem::offset_of!(SpriteInstance, position),
        "`position` must sit at the same offset in both sprite records"
    );
    assert!(
        std::mem::offset_of!(GpuSpriteInstance, size) == std::mem::offset_of!(SpriteInstance, size),
        "`size` must sit at the same offset in both sprite records"
    );
    assert!(
        std::mem::offset_of!(GpuSpriteInstance, color)
            == std::mem::offset_of!(SpriteInstance, color),
        "`color` must sit at the same offset in both sprite records"
    );
};

impl From<SpriteInstance> for GpuSpriteInstance {
    fn from(instance: SpriteInstance) -> Self {
        Self {
            position: instance.position,
            size: instance.size,
            color: instance.color,
        }
    }
}

/// Uniform holding the viewport size, used to convert pixel coordinates to
/// normalized device coordinates in the vertex shader.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ViewportUniform {
    /// Viewport width in pixels.
    width: f32,
    /// Viewport height in pixels.
    height: f32,
    /// Padding to 16-byte alignment required by WGSL uniform buffers.
    _padding: [f32; 2],
}

// =============================================================================
// SpriteRenderer
// =============================================================================

/// Draws every `(Position, Sprite)` entity as an instanced colored quad.
///
/// Owns a render pipeline and reusable GPU buffers; does not own a window,
/// surface, device, or queue - callers create those and pass them in.
pub struct SpriteRenderer {
    /// Compiled render pipeline for the sprite shader.
    pipeline: wgpu::RenderPipeline,
    /// GPU buffer holding the viewport uniform.
    viewport_buffer: wgpu::Buffer,
    /// Bind group exposing the viewport uniform to the vertex stage.
    viewport_bind_group: wgpu::BindGroup,
    /// GPU vertex buffer holding per-instance sprite data.
    instance_buffer: wgpu::Buffer,
    /// Number of `GpuSpriteInstance` slots currently allocated on the GPU.
    instance_capacity: usize,
    /// Background the render pass clears to, resolved for the target format.
    clear_color: wgpu::Color,
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
            array_stride: std::mem::size_of::<GpuSpriteInstance>() as u64,
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
            size: (Self::INITIAL_CAPACITY * std::mem::size_of::<GpuSpriteInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            viewport_buffer,
            viewport_bind_group,
            instance_buffer,
            instance_capacity: Self::INITIAL_CAPACITY,
            clear_color: viewport_background(target_format),
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
        self.render_in_viewport(
            world,
            device,
            queue,
            view,
            RenderViewport::full(viewport_width, viewport_height),
        );
    }

    /// Draw sprites within one physical region of a larger render target.
    ///
    /// The render pass clears the complete target to the opaque viewport
    /// background before drawing; sprite pixels are transformed and clipped to
    /// `viewport`. The clear covers the whole target, not just `viewport`, so a
    /// UI framework compositing above this surface sees the background colour
    /// wherever its own panels do not paint - it used to see through to
    /// whatever was behind.
    pub fn render_in_viewport(
        &mut self,
        world: &mut World,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &wgpu::TextureView,
        viewport: RenderViewport,
    ) {
        self.render_in_viewport_with_resolution(
            world,
            device,
            queue,
            view,
            viewport,
            VirtualResolution::new(viewport.width.max(1) as f32, viewport.height.max(1) as f32),
        );
    }

    /// Draw sprites in a physical viewport using a stable logical resolution.
    ///
    /// Sprite positions and sizes are interpreted in `virtual_resolution`.
    /// wgpu then scales that complete coordinate space to fill `viewport`, and
    /// the scissor rectangle prevents pixels from escaping the dock panel.
    pub fn render_in_viewport_with_resolution(
        &mut self,
        world: &mut World,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &wgpu::TextureView,
        viewport: RenderViewport,
        virtual_resolution: VirtualResolution,
    ) {
        debug_assert!(virtual_resolution.is_valid());

        // Step 1: Collect the instanced quad data for every sprite in the world.
        let instances: Vec<GpuSpriteInstance> = world
            .sprite_instances()
            .into_iter()
            .map(GpuSpriteInstance::from)
            .collect();

        // Step 2: Upload the viewport uniform so the vertex shader can map
        // pixel coordinates into normalized device coordinates.
        queue.write_buffer(
            &self.viewport_buffer,
            0,
            bytemuck::bytes_of(&ViewportUniform {
                width: virtual_resolution.width,
                height: virtual_resolution.height,
                _padding: [0.0, 0.0],
            }),
        );

        // Step 3: Upload the instance data, growing the GPU buffer to a power
        // of two when the world grows beyond the current capacity.
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

        // Step 4: Encode and submit a render pass that clears the target and
        // draws the instanced quads inside the scissored viewport region.
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
                        load: wgpu::LoadOp::Clear(self.clear_color),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            if !instances.is_empty() && viewport.width > 0 && viewport.height > 0 {
                render_pass.set_pipeline(&self.pipeline);
                render_pass.set_viewport(
                    viewport.x as f32,
                    viewport.y as f32,
                    viewport.width as f32,
                    viewport.height as f32,
                    0.0,
                    1.0,
                );
                render_pass.set_scissor_rect(
                    viewport.x,
                    viewport.y,
                    viewport.width,
                    viewport.height,
                );
                render_pass.set_bind_group(0, &self.viewport_bind_group, &[]);
                render_pass.set_vertex_buffer(0, self.instance_buffer.slice(..));
                render_pass.draw(0..6, 0..instances.len() as u32);
            }
        }

        queue.submit(Some(encoder.finish()));
    }
}
