//! Draws a [`DrawList`] with wgpu: one pipeline, textures, clips.
//!
//! # Responsibilities
//!
//! - [`UiRenderer`]: owns the render pipeline, the glyph-atlas texture and
//!   the registered external textures; uploads atlas changes and draws a
//!   [`DrawList`] into any render target.
//! - Applies the monitor's scale factor, the list's nested clip rectangles
//!   (as scissor rectangles) and correct sRGB blending.
//!
//! # Design
//!
//! The renderer is an `rlib` compiled into its user, which owns the device,
//! queue and target (plan decision D8). It keeps no camera, no window and no
//! frame state beyond its GPU resources; everything per-frame comes in
//! through [`UiRenderer::render`].
//!
//! Colour correctness: `pill_ui` colours are sRGB with straight alpha. The
//! shader converts vertex colours to linear, the textures are created as
//! `Rgba8UnormSrgb` (so sampling decodes them), and the caller is expected to
//! pass an sRGB target format - the hardware then encodes linear to sRGB
//! exactly once. Blending is the standard straight-alpha
//! `src * src_alpha + dst * (1 - src_alpha)`.

// Standard library
use std::collections::HashMap;

// External crates
use bytemuck::{Pod, Zeroable};
use pill_core::math::Vector2f;
use pill_ui::{Atlas, Color, DrawCommand, DrawList, Rect, Region, TextureId, UiVertex};
use wgpu::util::DeviceExt;

// =============================================================================
// Shader
// =============================================================================

/// The whole pipeline in one WGSL source: logical pixels to clip space in
/// the vertex stage, linear light out of the fragment stage.
const UI_SHADER: &str = r#"
struct Globals {
    // Physical target size in .xy and the monitor scale factor in .z.
    viewport: vec4<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(1) @binding(0) var ui_texture: texture_2d<f32>;
@group(1) @binding(1) var ui_sampler: sampler;

struct VertexIn {
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) local: vec2<f32>,
    @location(4) shape: vec4<f32>,
};

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) local: vec2<f32>,
    @location(3) shape: vec4<f32>,
};

// The sRGB to linear-light transfer function, per channel.
fn srgb_to_linear(color: vec3<f32>) -> vec3<f32> {
    let low = color / 12.92;
    let high = pow((color + 0.055) / 1.055, vec3<f32>(2.4));
    return select(low, high, color > vec3<f32>(0.04045));
}

@vertex
fn vs_main(in: VertexIn) -> VertexOut {
    let physical = in.position * globals.viewport.z;
    let normalized = physical / globals.viewport.xy;
    var out: VertexOut;
    out.position = vec4<f32>(normalized.x * 2.0 - 1.0, 1.0 - normalized.y * 2.0, 0.0, 1.0);
    out.uv = in.uv;
    out.color = in.color;
    out.local = in.local;
    out.shape = in.shape;
    return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let texel = textureSample(ui_texture, ui_sampler, in.uv);
    var coverage = 1.0;
    let radius = in.shape.z;
    let softness = in.shape.w;
    if (radius > 0.0 || softness > 0.0) {
        // The signed distance to the rounded rectangle's edge, from the
        // interpolated quad-local position and the shape's half extents.
        let half = in.shape.xy;
        let point = in.local - half;
        let offset = abs(point) - half + vec2<f32>(radius);
        let distance = length(max(offset, vec2<f32>(0.0))) + min(max(offset.x, offset.y), 0.0) - radius;
        if (softness > 0.0) {
            // A soft shape (a shadow): full `softness` pixels inside the
            // edge, nothing at and beyond it.
            let falloff = clamp(-distance / softness, 0.0, 1.0);
            coverage = falloff * falloff * (3.0 - 2.0 * falloff);
        } else {
            // A rounded fill: antialias the one-pixel boundary.
            let aa = max(fwidth(distance), 0.0001);
            coverage = 1.0 - smoothstep(0.0, aa, distance);
        }
    }
    // Both the sampled texel and the vertex colour are linear light already;
    // the sRGB target encodes the result.
    let linear = srgb_to_linear(in.color.rgb) * texel.rgb;
    return vec4<f32>(linear, in.color.a * texel.a * coverage);
}
"#;

// =============================================================================
// GpuVertex
// =============================================================================

/// One vertex as the GPU sees it: position, texture coordinate, colour -
/// `f32` components in a fixed layout the pipeline declares.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct GpuVertex {
    position: [f32; 2],
    uv: [f32; 2],
    color: [f32; 4],
    local: [f32; 2],
    shape: [f32; 4],
}

impl From<&UiVertex> for GpuVertex {
    fn from(vertex: &UiVertex) -> GpuVertex {
        GpuVertex {
            position: [vertex.position.x, vertex.position.y],
            uv: [vertex.uv.x, vertex.uv.y],
            color: [
                vertex.color.r,
                vertex.color.g,
                vertex.color.b,
                vertex.color.a,
            ],
            local: [vertex.local.x, vertex.local.y],
            shape: vertex.shape,
        }
    }
}

/// The vertex layout matching [`GpuVertex`].
const VERTEX_ATTRIBUTES: [wgpu::VertexAttribute; 5] = wgpu::vertex_attr_array![
    0 => Float32x2,
    1 => Float32x2,
    2 => Float32x4,
    3 => Float32x2,
    4 => Float32x4
];

// =============================================================================
// UiRenderer
// =============================================================================

/// GPU resources for drawing `pill_ui` draw lists.
///
/// One renderer belongs to one device and one target format. The caller
/// uploads atlas changes once per frame ([`Self::upload_atlas`]) and calls
/// [`Self::render`] for each target the list draws into.
pub struct UiRenderer {
    pipeline: wgpu::RenderPipeline,
    globals_buffer: wgpu::Buffer,
    globals_bind_group: wgpu::BindGroup,
    texture_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    atlas_texture: wgpu::Texture,
    atlas_bind_group: wgpu::BindGroup,
    atlas_size: (u32, u32),
    atlas_uploaded: bool,
    solid_bind_group: wgpu::BindGroup,
    externals: HashMap<u64, wgpu::BindGroup>,
}

impl UiRenderer {
    /// Build the pipeline and the atlas texture for `target_format`.
    ///
    /// `atlas_size` must be the size of the [`Atlas`] the caller will upload
    /// (`Atlas::new` with the same numbers). Pass an sRGB target format
    /// (`Rgba8UnormSrgb`, or a surface format's sRGB variant); the shader
    /// assumes the target encodes linear light. `queue` initializes the
    /// white texel solid quads sample.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target_format: wgpu::TextureFormat,
        atlas_size: (u32, u32),
    ) -> UiRenderer {
        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pill_ui globals layout"),
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
        let texture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pill_ui texture layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let globals_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pill_ui globals"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pill_ui globals bind group"),
            layout: &globals_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buffer.as_entire_binding(),
            }],
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("pill_ui sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pill_ui shader"),
            source: wgpu::ShaderSource::Wgsl(UI_SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pill_ui pipeline layout"),
            bind_group_layouts: &[&globals_layout, &texture_layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("pill_ui pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<GpuVertex>() as wgpu::BufferAddress,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &VERTEX_ATTRIBUTES,
                }],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::SrcAlpha,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            multiview: None,
            cache: None,
        });

        let atlas_texture = create_srgb_texture(device, "pill_ui atlas", atlas_size);
        let atlas_view = atlas_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let atlas_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pill_ui atlas bind group"),
            layout: &texture_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        // The `Solid` texture: one white texel, so a solid quad is the vertex
        // colour times 1. The shader needs no branch for it, and all draws
        // share one pipeline.
        let solid_texture = create_srgb_texture(device, "pill_ui solid", (1, 1));
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &solid_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[255, 255, 255, 255],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let solid_view = solid_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let solid_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pill_ui solid bind group"),
            layout: &texture_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&solid_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        UiRenderer {
            pipeline,
            globals_buffer,
            globals_bind_group,
            texture_layout,
            sampler,
            atlas_texture,
            atlas_bind_group,
            atlas_size,
            atlas_uploaded: false,
            solid_bind_group,
            externals: HashMap::new(),
        }
    }

    /// The atlas size this renderer was built for.
    pub fn atlas_size(&self) -> (u32, u32) {
        self.atlas_size
    }

    /// Upload what the atlas changed since the last upload.
    ///
    /// The first call uploads the whole image; later calls upload only the
    /// regions [`Atlas::take_updated_regions`] reports. `atlas` must be the
    /// size this renderer was built for.
    pub fn upload_atlas(&mut self, queue: &wgpu::Queue, atlas: &mut Atlas) {
        assert_eq!(
            (atlas.width(), atlas.height()),
            self.atlas_size,
            "the atlas is {}x{} but this renderer was built for {}x{}",
            atlas.width(),
            atlas.height(),
            self.atlas_size.0,
            self.atlas_size.1
        );
        let regions = atlas.take_updated_regions();
        if !self.atlas_uploaded {
            self.atlas_uploaded = true;
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.atlas_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                atlas.pixels(),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.atlas_size.0 * 4),
                    rows_per_image: Some(self.atlas_size.1),
                },
                wgpu::Extent3d {
                    width: self.atlas_size.0,
                    height: self.atlas_size.1,
                    depth_or_array_layers: 1,
                },
            );
            return;
        }
        for region in regions {
            upload_atlas_region(queue, &self.atlas_texture, atlas, region);
        }
    }

    /// Register (or replace) the external texture `id` samples.
    ///
    /// `rgba` is `size.0 * size.1 * 4` bytes of sRGB-encoded RGBA8, the same
    /// space [`Color`] uses; the created texture decodes it while sampling.
    pub fn set_external(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        id: u64,
        size: (u32, u32),
        rgba: &[u8],
    ) {
        assert_eq!(
            rgba.len(),
            size.0 as usize * size.1 as usize * 4,
            "an external texture needs exactly width * height * 4 bytes"
        );
        let texture = create_srgb_texture(device, "pill_ui external", size);
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(size.0 * 4),
                rows_per_image: Some(size.1),
            },
            wgpu::Extent3d {
                width: size.0,
                height: size.1,
                depth_or_array_layers: 1,
            },
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pill_ui external bind group"),
            layout: &self.texture_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        self.externals.insert(id, bind_group);
    }

    /// Forget an external texture; draws naming it are skipped from now on.
    pub fn remove_external(&mut self, id: u64) -> bool {
        self.externals.remove(&id).is_some()
    }

    /// Draw `list` into `target`.
    ///
    /// `target_size` is the target in physical pixels and `scale_factor` the
    /// monitor scale the list's logical pixels are in. `clear` clears the
    /// target first (the colour is converted from sRGB to linear the way a
    /// clear value must be); `None` loads whatever is already there, for a UI
    /// pass drawn over a scene.
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        target_size: (u32, u32),
        scale_factor: f32,
        list: &DrawList,
        clear: Option<Color>,
    ) {
        if list.is_empty() && clear.is_none() {
            return;
        }

        queue.write_buffer(
            &self.globals_buffer,
            0,
            bytemuck::cast_slice(&[
                target_size.0 as f32,
                target_size.1 as f32,
                scale_factor,
                0.0,
            ]),
        );
        let vertices: Vec<GpuVertex> = list.vertices.iter().map(GpuVertex::from).collect();
        let vertex_buffer = (!vertices.is_empty()).then(|| {
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("pill_ui vertices"),
                contents: bytemuck::cast_slice(&vertices),
                usage: wgpu::BufferUsages::VERTEX,
            })
        });
        let index_buffer = (!list.indices.is_empty()).then(|| {
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("pill_ui indices"),
                contents: bytemuck::cast_slice(&list.indices),
                usage: wgpu::BufferUsages::INDEX,
            })
        });

        let load = clear.map_or(wgpu::LoadOp::Load, |color| {
            wgpu::LoadOp::Clear(linear_clear_value(color))
        });
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("pill_ui"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                resolve_target: None,
                ops: wgpu::Operations {
                    load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        let (Some(vertex_buffer), Some(index_buffer)) = (vertex_buffer, index_buffer) else {
            return;
        };

        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.globals_bind_group, &[]);
        pass.set_vertex_buffer(0, vertex_buffer.slice(..));
        pass.set_index_buffer(index_buffer.slice(..), wgpu::IndexFormat::Uint32);

        // The list's clips nest; the innermost one is the pass's scissor and
        // the stack restores the previous one on every pop.
        let full_clip = Rect::new(
            Vector2f::ZERO,
            Vector2f::new(
                target_size.0 as f32 / scale_factor,
                target_size.1 as f32 / scale_factor,
            ),
        );
        let mut clip = full_clip;
        let mut stack: Vec<Rect> = Vec::new();

        for command in &list.commands {
            match command {
                DrawCommand::PushClip(rect) => {
                    stack.push(clip);
                    clip = clip
                        .intersection(rect)
                        .unwrap_or(Rect::from_min_size(Vector2f::ZERO, Vector2f::ZERO));
                }
                DrawCommand::PopClip => {
                    clip = stack.pop().unwrap_or(full_clip);
                }
                DrawCommand::Draw {
                    texture,
                    first_index,
                    index_count,
                } => {
                    let Some(bind_group) = self.bind_group_for(texture) else {
                        continue;
                    };
                    let Some(scissor) = physical_scissor(&clip, scale_factor, target_size) else {
                        continue;
                    };
                    pass.set_scissor_rect(scissor[0], scissor[1], scissor[2], scissor[3]);
                    pass.set_bind_group(1, bind_group, &[]);
                    pass.draw_indexed(*first_index..first_index + index_count, 0, 0..1);
                }
            }
        }
    }

    /// The bind group a draw samples from, or nothing for an unregistered
    /// external texture (the draw is skipped).
    fn bind_group_for(&self, texture: &TextureId) -> Option<&wgpu::BindGroup> {
        match texture {
            TextureId::Atlas => Some(&self.atlas_bind_group),
            TextureId::Solid => Some(&self.solid_bind_group),
            TextureId::External(id) => self.externals.get(id),
        }
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// An `Rgba8UnormSrgb` texture the shader can sample and the queue can
/// write: sampling decodes sRGB to linear, matching the shader's model.
fn create_srgb_texture(device: &wgpu::Device, label: &str, size: (u32, u32)) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

/// Upload one dirtied atlas region from the atlas image to the texture.
fn upload_atlas_region(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    atlas: &Atlas,
    region: Region,
) {
    let row_bytes = region.width as usize * 4;
    let atlas_stride = atlas.width() as usize * 4;
    let mut rows = Vec::with_capacity(row_bytes * region.height as usize);
    for row in 0..region.height as usize {
        let start = (region.y as usize + row) * atlas_stride + region.x as usize * 4;
        rows.extend_from_slice(&atlas.pixels()[start..start + row_bytes]);
    }
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d {
                x: region.x,
                y: region.y,
                z: 0,
            },
            aspect: wgpu::TextureAspect::All,
        },
        &rows,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(region.width * 4),
            rows_per_image: Some(region.height),
        },
        wgpu::Extent3d {
            width: region.width,
            height: region.height,
            depth_or_array_layers: 1,
        },
    );
}

/// A logical-pixel clip as the scissor rectangle a render pass wants, in
/// physical pixels clamped to the target; `None` when nothing would be
/// drawn (an empty or fully outside clip).
fn physical_scissor(clip: &Rect, scale_factor: f32, target_size: (u32, u32)) -> Option<[u32; 4]> {
    let min_x = (clip.min.x * scale_factor).floor().max(0.0);
    let min_y = (clip.min.y * scale_factor).floor().max(0.0);
    let max_x = (clip.max.x * scale_factor).ceil().min(target_size.0 as f32);
    let max_y = (clip.max.y * scale_factor).ceil().min(target_size.1 as f32);
    if max_x <= min_x || max_y <= min_y {
        return None;
    }
    Some([
        min_x as u32,
        min_y as u32,
        (max_x - min_x) as u32,
        (max_y - min_y) as u32,
    ])
}

/// The sRGB to linear-light transfer function, per channel (the same curve
/// the shader applies), for the one place the CPU writes colour: the clear.
fn srgb_to_linear(value: f32) -> f32 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// A `pill_ui` sRGB colour as the linear clear value a render pass wants.
fn linear_clear_value(color: Color) -> wgpu::Color {
    wgpu::Color {
        r: srgb_to_linear(color.r) as f64,
        g: srgb_to_linear(color.g) as f64,
        b: srgb_to_linear(color.b) as f64,
        a: color.a as f64,
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_scissor_rounds_outward_and_clamps_to_the_target() {
        let target = (100u32, 80u32);
        let clip = Rect::new(Vector2f::new(1.2, 2.7), Vector2f::new(30.5, 40.1));
        assert_eq!(
            physical_scissor(&clip, 1.0, target),
            Some([1, 2, 30, 39]),
            "min floors, max ceils"
        );
        let scaled = Rect::new(Vector2f::new(1.2, 2.7), Vector2f::new(30.5, 40.1));
        assert_eq!(
            physical_scissor(&scaled, 2.0, target),
            Some([2, 5, 59, 75]),
            "the scale factor multiplies before rounding"
        );

        // The clip runs off the top-left and past the bottom-right.
        let outside = Rect::new(Vector2f::new(-10.0, -10.0), Vector2f::new(500.0, 500.0));
        assert_eq!(
            physical_scissor(&outside, 1.0, target),
            Some([0, 0, 100, 80])
        );
    }

    #[test]
    fn physical_scissor_is_none_for_empty_or_outside_clips() {
        let target = (100u32, 80u32);
        let empty = Rect::from_min_size(Vector2f::ZERO, Vector2f::ZERO);
        assert_eq!(physical_scissor(&empty, 1.0, target), None);
        let outside = Rect::new(Vector2f::new(200.0, 200.0), Vector2f::new(300.0, 300.0));
        assert_eq!(physical_scissor(&outside, 1.0, target), None);
        let zero_width = Rect::new(Vector2f::new(10.0, 10.0), Vector2f::new(10.0, 20.0));
        assert_eq!(
            physical_scissor(&zero_width, 1.0, target),
            None,
            "a clip with no area draws nothing"
        );
    }

    #[test]
    fn linear_clear_value_matches_the_standard_curve() {
        let black = linear_clear_value(Color::rgba(0.0, 0.0, 0.0, 0.5));
        assert_eq!((black.r, black.g, black.b, black.a), (0.0, 0.0, 0.0, 0.5));
        let white = linear_clear_value(Color::WHITE);
        assert_eq!((white.r, white.g, white.b, white.a), (1.0, 1.0, 1.0, 1.0));
        let middle = linear_clear_value(Color::rgba(0.5, 0.5, 0.5, 1.0));
        let expected = srgb_to_linear(0.5) as f64;
        assert!(
            (middle.r - expected).abs() < 1e-12,
            "0.5 in sRGB is {expected} in linear light"
        );
    }

    #[test]
    fn gpu_vertex_matches_the_declared_layout() {
        assert_eq!(std::mem::size_of::<GpuVertex>(), 56);
        let vertex = UiVertex {
            position: Vector2f::new(1.0, 2.0),
            uv: Vector2f::new(0.25, 0.75),
            color: Color::rgba(0.1, 0.2, 0.3, 0.4),
            local: Vector2f::new(3.0, 4.0),
            shape: [5.0, 6.0, 7.0, 8.0],
        };
        let gpu = GpuVertex::from(&vertex);
        assert_eq!(gpu.position, [1.0, 2.0]);
        assert_eq!(gpu.uv, [0.25, 0.75]);
        assert_eq!(gpu.color, [0.1, 0.2, 0.3, 0.4]);
        assert_eq!(gpu.local, [3.0, 4.0]);
        assert_eq!(gpu.shape, [5.0, 6.0, 7.0, 8.0]);
    }
}
