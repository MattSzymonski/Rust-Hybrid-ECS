//! Draws a [`DrawList`] with wgpu: one pipeline, textures, clips.
//!
//! # Responsibilities
//!
//! - [`UiRenderer`]: owns the render pipeline, the atlas texture and
//!   the registered external textures; uploads texture changes and draws a
//!   surface's [`DrawList`]s into its target.
//! - Applies each list's scale factor, its nested clip rectangles (as scissor
//!   rectangles) and correct sRGB blending.
//!
//! # Design
//!
//! The painter uses the renderer module's device, queue and target (plan
//! decision D8). It keeps no camera, no window and no frame state beyond its
//! GPU resources; everything per-frame comes in through
//! [`UiRenderer::render`], once per surface: every list for that surface goes
//! into one pair of reused buffers, since a buffer can only be written once
//! per submission (see [`StreamBuffer`]).
//!
//! Textures arrive as a versioned set ([`UiRenderer::apply_texture_set`]): the
//! renderer remembers the revision it applied per texture and uploads
//! whenever the carried revision is newer, so an instance rebuilt mid-run -
//! and therefore remembering nothing - re-uploads every texture the set
//! carries instead of drawing with empty textures (W-0028 D3).
//!
//! Colour correctness: `pill_draw` colours are sRGB with straight alpha. The
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
use pill_draw::{
    DrawCommand, DrawList, Rect, Region, TextureId, UiTexture, UiTextureFilter, UiTextureSet,
    UiVertex,
};

// Current crate
use crate::stream_buffer::StreamBuffer;

// =============================================================================
// Shader
// =============================================================================

/// The whole pipeline in one WGSL source: physical pixels to clip space in
/// the vertex stage, linear light out of the fragment stage.
const UI_SHADER: &str = r#"
struct Globals {
    // Physical target size in .xy; .zw unused.
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
    let normalized = in.position / globals.viewport.xy;
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
    // The signed distance to the rounded rectangle's edge, from the
    // interpolated quad-local position and the shape's half extents. Both
    // this and the derivative below are evaluated in uniform control flow:
    // WGSL requires `fwidth` to stay outside of non-uniform branches (the
    // WebGL2 backend rejects the shader otherwise), while native backends
    // happen to tolerate it.
    let half = in.shape.xy;
    let point = in.local - half;
    let offset = abs(point) - half + vec2<f32>(radius);
    let distance =
        length(max(offset, vec2<f32>(0.0))) + min(max(offset.x, offset.y), 0.0) - radius;
    let edge_width = max(fwidth(distance), 0.0001);
    if (radius > 0.0 || softness > 0.0) {
        if (softness > 0.0) {
            // A soft shape (a shadow): full `softness` pixels inside the
            // edge, nothing at and beyond it.
            let falloff = clamp(-distance / softness, 0.0, 1.0);
            coverage = falloff * falloff * (3.0 - 2.0 * falloff);
        } else {
            // A rounded fill: antialias the one-pixel boundary.
            coverage = 1.0 - smoothstep(0.0, edge_width, distance);
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

/// One vertex as the GPU sees it: position in physical pixels, texture
/// coordinate, colour - `f32` components in a fixed layout the pipeline
/// declares.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct GpuVertex {
    position: [f32; 2],
    uv: [f32; 2],
    color: [f32; 4],
    local: [f32; 2],
    shape: [f32; 4],
}

impl GpuVertex {
    /// `vertex` with its logical position scaled to physical pixels, so lists
    /// of different scale factors can share one buffer and one pass.
    fn scaled(vertex: &UiVertex, scale_factor: f32) -> GpuVertex {
        GpuVertex {
            position: [
                vertex.position.x * scale_factor,
                vertex.position.y * scale_factor,
            ],
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

/// GPU resources for drawing `pill_draw` draw lists.
///
/// One renderer belongs to one device and one target format. The caller
/// uploads texture changes once per frame ([`Self::apply_texture_set`]) and calls
/// [`Self::render`] for each target the list draws into.
pub struct UiRenderer {
    pipeline: wgpu::RenderPipeline,
    globals_buffer: wgpu::Buffer,
    globals_bind_group: wgpu::BindGroup,
    texture_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// The sampler [`UiTextureFilter::Nearest`] asks for, beside the linear
    /// one the crate was built with.
    nearest_sampler: wgpu::Sampler,
    atlas_texture: wgpu::Texture,
    atlas_bind_group: wgpu::BindGroup,
    atlas_size: (u32, u32),
    atlas_uploaded: bool,
    solid_bind_group: wgpu::BindGroup,
    externals: HashMap<u64, ExternalTexture>,
    /// The revision this instance applied per texture; a rebuilt instance
    /// starts empty and therefore re-uploads everything.
    applied_revisions: HashMap<TextureId, u64>,
    /// Every list of the surface being drawn: its vertices, then its indices.
    vertices: StreamBuffer,
    indices: StreamBuffer,
}

impl UiRenderer {
    /// Build the pipeline and the atlas texture for `target_format`.
    ///
    /// `atlas_size` is the atlas page the painter starts at (`pill_draw`'s
    /// same numbers); a later page growth recreates the texture inside
    /// `DEFAULT_ATLAS_LENGTH`). Pass an sRGB target format
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
            label: Some("pill_draw globals layout"),
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
            label: Some("pill_draw texture layout"),
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
            label: Some("pill_draw globals"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pill_draw globals bind group"),
            layout: &globals_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buffer.as_entire_binding(),
            }],
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("pill_draw sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let nearest_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("pill_draw nearest sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pill_draw shader"),
            source: wgpu::ShaderSource::Wgsl(UI_SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pill_draw pipeline layout"),
            bind_group_layouts: &[Some(&globals_layout), Some(&texture_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("pill_draw pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<GpuVertex>() as wgpu::BufferAddress,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &VERTEX_ATTRIBUTES,
                })],
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
            multiview_mask: None,
            cache: None,
        });

        let (atlas_texture, atlas_bind_group) =
            atlas_resources(device, &texture_layout, &sampler, atlas_size);

        // The `Solid` texture: one white texel, so a solid quad is the vertex
        // colour times 1. The shader needs no branch for it, and all draws
        // share one pipeline.
        let solid_texture = create_srgb_texture(device, "pill_draw solid", (1, 1));
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
            label: Some("pill_draw solid bind group"),
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
            nearest_sampler,
            atlas_texture,
            atlas_bind_group,
            atlas_size,
            atlas_uploaded: false,
            solid_bind_group,
            externals: HashMap::new(),
            applied_revisions: HashMap::new(),
            vertices: StreamBuffer::new("pill_draw vertices", wgpu::BufferUsages::VERTEX),
            indices: StreamBuffer::new("pill_draw indices", wgpu::BufferUsages::INDEX),
        }
    }

    /// Upload every texture in `set` whose carried revision this instance has
    /// not applied, then free the ones it names.
    ///
    /// This is the reload-safe path (W-0028 D3): a rebuilt instance has an
    /// empty revision table, so the first set it sees uploads every texture
    /// from the whole images the set carries - no back-channel, no cadence.
    /// A live instance only uploads what changed.
    pub fn apply_texture_set(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        set: &UiTextureSet,
    ) {
        for texture in &set.textures {
            if self.applied_revisions.get(&texture.id) == Some(&texture.revision) {
                continue;
            }
            self.apply_texture(device, queue, texture);
            self.applied_revisions.insert(texture.id, texture.revision);
        }
        for id in &set.frees {
            self.free_texture(id);
        }
    }

    /// Upload one texture at its carried revision.
    ///
    /// The whole image, when the revision carries one, is the authoritative
    /// content: it is uploaded whether or not this instance already holds the
    /// texture, which is what makes a rebuilt instance catch up in one frame.
    /// Patches are the cheap path for a revision that carries no whole image,
    /// and only mean anything for a texture the instance already holds - a
    /// patch for a texture it never applied is reported and skipped rather
    /// than drawn as garbage.
    fn apply_texture(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, texture: &UiTexture) {
        let size = (texture.width, texture.height);
        let sampler = self.sampler_for(texture.filter);
        match texture.id {
            TextureId::Atlas => {
                if size != self.atlas_size {
                    let (created, bind_group) =
                        atlas_resources(device, &self.texture_layout, &sampler, size);
                    self.atlas_texture = created;
                    self.atlas_bind_group = bind_group;
                    self.atlas_size = size;
                    self.atlas_uploaded = false;
                }
                match texture.full_image() {
                    Some(pixels) => {
                        upload_whole_texture(queue, &self.atlas_texture, size, pixels);
                        self.atlas_uploaded = true;
                    }
                    None if self.atlas_uploaded => {
                        for (region, pixels) in texture.patches() {
                            upload_region_pixels(queue, &self.atlas_texture, region, pixels);
                        }
                    }
                    None => report_orphan_patch("atlas"),
                }
            }
            TextureId::External(id) => {
                let existing_size = self.externals.get(&id).map(|entry| entry.size);
                let target = match texture.full_image() {
                    Some(pixels) => {
                        // Reuse the texture when it is the right size, so a
                        // live instance does not recreate it every revision.
                        let reuse = existing_size == Some(size);
                        let created = if reuse {
                            self.externals
                                .remove(&id)
                                .map(|entry| entry.texture)
                                .expect("size came from the entry")
                        } else {
                            create_srgb_texture(device, "pill_draw external", size)
                        };
                        upload_whole_texture(queue, &created, size, pixels);
                        Some(created)
                    }
                    None => match self.externals.get(&id) {
                        Some(entry) if entry.size == size => {
                            for (region, pixels) in texture.patches() {
                                upload_region_pixels(queue, &entry.texture, region, pixels);
                            }
                            None
                        }
                        _ => {
                            report_orphan_patch("external");
                            None
                        }
                    },
                };
                if let Some(created) = target {
                    let view = created.create_view(&wgpu::TextureViewDescriptor::default());
                    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("pill_draw external bind group"),
                        layout: &self.texture_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(&view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::Sampler(&sampler),
                            },
                        ],
                    });
                    self.externals.insert(
                        id,
                        ExternalTexture {
                            texture: created,
                            bind_group,
                            size,
                        },
                    );
                }
            }
            TextureId::Solid => {}
        }
    }

    /// The sampler a filter mode asks for.
    fn sampler_for(&self, filter: UiTextureFilter) -> wgpu::Sampler {
        match filter {
            UiTextureFilter::Nearest => self.nearest_sampler.clone(),
            UiTextureFilter::Linear => self.sampler.clone(),
        }
    }

    /// Drop a freed texture, so draws naming it are skipped from now on.
    fn free_texture(&mut self, id: &TextureId) {
        self.applied_revisions.remove(id);
        match id {
            TextureId::Atlas => {
                // The atlas texture stays allocated (the pipeline's bind group
                // layout is fixed); dropping the revision makes the next set
                // that names it upload again.
                self.atlas_uploaded = false;
            }
            TextureId::External(external) => {
                self.externals.remove(external);
            }
            TextureId::Solid => {}
        }
    }

    /// Draw `lists` into `target`, in order, over what it already holds.
    ///
    /// `target_size` is the target in physical pixels; each list comes with
    /// the monitor scale its logical pixels are in. Call this once per
    /// surface with every list for it: the lists share one upload, because
    /// the painter's buffers can be written only once per submission.
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        target_size: (u32, u32),
        lists: &[(&DrawList, f32)],
    ) {
        if lists.iter().all(|(list, _)| list.is_empty()) {
            return;
        }

        queue.write_buffer(
            &self.globals_buffer,
            0,
            bytemuck::cast_slice(&[target_size.0 as f32, target_size.1 as f32, 0.0, 0.0]),
        );
        // One upload for the surface: each list's vertices after the previous
        // list's, its indices unchanged and offset by `base_vertex` per draw.
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        let mut placements = Vec::with_capacity(lists.len());
        for (list, scale_factor) in lists {
            placements.push((vertices.len() as i32, indices.len() as u32));
            vertices.extend(
                list.vertices
                    .iter()
                    .map(|vertex| GpuVertex::scaled(vertex, *scale_factor)),
            );
            indices.extend_from_slice(&list.indices);
        }
        let vertex_buffer = self
            .vertices
            .write(device, queue, bytemuck::cast_slice(&vertices))
            .cloned();
        let index_buffer = self
            .indices
            .write(device, queue, bytemuck::cast_slice(&indices))
            .cloned();
        let (Some(vertex_buffer), Some(index_buffer)) = (vertex_buffer, index_buffer) else {
            return;
        };

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("pill_draw"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.globals_bind_group, &[]);
        pass.set_vertex_buffer(0, vertex_buffer.slice(..));
        pass.set_index_buffer(index_buffer.slice(..), wgpu::IndexFormat::Uint32);

        for ((list, scale_factor), (base_vertex, index_offset)) in lists.iter().zip(placements) {
            // The list's clips nest; the innermost one is the pass's scissor
            // and the stack restores the previous one on every pop.
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
                        let Some(scissor) = physical_scissor(&clip, *scale_factor, target_size)
                        else {
                            continue;
                        };
                        pass.set_scissor_rect(scissor[0], scissor[1], scissor[2], scissor[3]);
                        pass.set_bind_group(1, bind_group, &[]);
                        let first = index_offset + first_index;
                        pass.draw_indexed(first..first + index_count, base_vertex, 0..1);
                    }
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
            TextureId::External(id) => self.externals.get(id).map(|entry| &entry.bind_group),
        }
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// One external texture: what it is, how it is bound, and its texel size.
struct ExternalTexture {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    size: (u32, u32),
}

/// Report a patch that arrived for a texture the renderer has never applied.
///
/// The producer carries the whole image with every changed revision, so this
/// only fires when a producer patches a texture it never sent; one line says
/// so instead of drawing with garbage.
fn report_orphan_patch(kind: &str) {
    pill_core::warn!(
        target: pill_core::telemetry::telemetry_target::ECS,
        kind,
        "a UI texture patch arrived for a texture this renderer never applied"
    );
}

/// Upload `pixels` as the whole 2D texture `texture` of `size`.
fn upload_whole_texture(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    size: (u32, u32),
    pixels: &[u8],
) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        pixels,
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
}

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

/// The atlas texture at `size` and the bind group sampling it.
///
/// The atlas grows in pages, so the pair is rebuilt when its size changes;
/// both the constructor and the upload path make it through here, keeping
/// the bind group's layout and sampler in one place.
fn atlas_resources(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    size: (u32, u32),
) -> (wgpu::Texture, wgpu::BindGroup) {
    let texture = create_srgb_texture(device, "pill_draw atlas", size);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("pill_draw atlas bind group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    });
    (texture, bind_group)
}

/// Upload one region's tightly packed RGBA8 rows to the atlas texture.
fn upload_region_pixels(queue: &wgpu::Queue, texture: &wgpu::Texture, region: Region, rows: &[u8]) {
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
        rows,
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

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use pill_draw::Color;

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
    fn gpu_vertex_matches_the_declared_layout() {
        assert_eq!(std::mem::size_of::<GpuVertex>(), 56);
        let vertex = UiVertex {
            position: Vector2f::new(1.0, 2.0),
            uv: Vector2f::new(0.25, 0.75),
            color: Color::rgba(0.1, 0.2, 0.3, 0.4),
            local: Vector2f::new(3.0, 4.0),
            shape: [5.0, 6.0, 7.0, 8.0],
        };
        let gpu = GpuVertex::scaled(&vertex, 2.0);
        assert_eq!(
            gpu.position,
            [2.0, 4.0],
            "the position is scaled to physical pixels"
        );
        assert_eq!(gpu.uv, [0.25, 0.75]);
        assert_eq!(gpu.color, [0.1, 0.2, 0.3, 0.4]);
        assert_eq!(gpu.local, [3.0, 4.0]);
        assert_eq!(gpu.shape, [5.0, 6.0, 7.0, 8.0]);
    }
}
