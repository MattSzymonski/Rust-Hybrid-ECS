//! Indexed instanced meshes rendered into linear HDR and tonemapped output.
//!
//! # Responsibilities
//!
//! - Owns mesh, background, and tonemap pipelines plus their intermediate targets.
//! - Prepares GPU assets and a reusable per-instance vertex stream.
//! - Batches sorted instances and reports CPU preparation/submission metrics.
//!
//! # Design
//!
//! The pass consumes an owned [`RenderFrame`] and never queries the world.
//! Scene shading writes RGBA16Float with Depth32Float depth; a second pass applies
//! exposure and an ACES-style curve to the presentation format. Surface sRGB
//! conversion is used when available, otherwise the shader performs it explicitly.
//!
//! Asset caches retain their accepted CPU snapshot so rejected uploads cannot mix
//! new material factors with old GPU maps. Instance storage grows geometrically
//! and is reused between frames. Shader replacements are validated by the outer
//! renderer before a candidate pass replaces the active one.

// Current crate
use crate::{RenderFrame, RenderViewport};

// External crates
use wgpu::util::DeviceExt;

// =============================================================================
// GPU Data Layouts
// =============================================================================

/// Packed 56-byte GPU vertex; its field order differs from the RMSH file layout.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    /// Local-space position.
    position: [f32; 3],
    /// Local-space surface normal.
    normal: [f32; 3],
    /// Texture coordinates for material maps.
    uv: [f32; 2],
    /// Local-space direction along increasing texture U.
    tangent: [f32; 3],
    /// Local-space direction along increasing texture V.
    bitangent: [f32; 3],
}

/// Packed 96-byte instance stream matching vertex locations 3 through 8.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Instance {
    /// Four column-major transform columns, one vertex attribute per column.
    model: [[f32; 4]; 4],
    /// Combined entity and material base-color factors.
    color: [f32; 4],
    /// Metallic and roughness factors followed by two unused floats.
    params: [f32; 4],
}

/// 176-byte uniform block matching the shader's `Frame` structure.
///
/// Vector fields occupy four floats each to respect uniform alignment.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct FrameUniform {
    /// Column-major view-projection matrix.
    vp: [[f32; 4]; 4],
    /// World-space camera position with a padding/homogeneous component.
    eye: [f32; 4],
    /// World-space light ray direction; the shader negates it toward the source.
    light_dir: [f32; 4],
    /// Linear directional radiance in RGB; the fourth component is unused.
    light_color: [f32; 4],
    /// Inverse projection-view transform for reconstructing background rays.
    inverse_vp: [[f32; 4]; 4],
}

// =============================================================================
// Allocation Helpers
// =============================================================================

/// Create and initialize a labelled GPU buffer from packed bytes.
fn buffer(d: &wgpu::Device, label: &str, data: &[u8], usage: wgpu::BufferUsages) -> wgpu::Buffer {
    d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some(label),
        contents: data,
        usage,
    })
}

/// Allocate a sampleable render target, clamping both dimensions to at least one.
fn target(d: &wgpu::Device, w: u32, h: u32, format: wgpu::TextureFormat) -> wgpu::Texture {
    d.create_texture(&wgpu::TextureDescriptor {
        label: Some("PBR target"),
        size: wgpu::Extent3d {
            width: w.max(1),
            height: h.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    })
}

// =============================================================================
// PBR Pass
// =============================================================================

/// Renderer-owned PBR pass, upload cache, and size-dependent targets.
pub struct PbrPipeline {
    /// Counters from the latest call to [`Self::render`].
    pub metrics: crate::api::RenderMetrics,
    /// Layout shared by every material bind group.
    material_layout: wgpu::BindGroupLayout,
    /// Layout shared by the selected environment textures.
    environment_layout: wgpu::BindGroupLayout,
    /// Accepted CPU generation and its matching GPU resources.
    assets: crate::gpu_assets::GpuAssets,
    /// Fullscreen environment pipeline with depth writes disabled.
    background: wgpu::RenderPipeline,
    /// Opaque and alpha-masked mesh pipeline writing HDR color and depth.
    pipeline: wgpu::RenderPipeline,
    /// Fullscreen conversion from HDR to the presentation format.
    tonemap: wgpu::RenderPipeline,
    /// Vertex buffer for the built-in sphere fallback.
    vertices: wgpu::Buffer,
    /// Index buffer for the built-in sphere fallback.
    indices: wgpu::Buffer,
    /// Index count of the sphere fallback.
    index_count: u32,
    /// Per-frame camera and directional light data.
    uniform: wgpu::Buffer,
    /// Binding for the per-frame uniform block.
    group: wgpu::BindGroup,
    /// Reusable GPU instance stream.
    instances: wgpu::Buffer,
    /// Number of instances that fit in the current GPU buffer.
    capacity: usize,
    /// Reusable CPU copy of the packed instance stream.
    staging: Vec<Instance>,
    /// Previously reported missing mesh/material pairs, preventing per-frame log spam.
    missing: std::collections::BTreeSet<(u64, u64)>,
    /// Last revision rejected for device limits, also used to deduplicate diagnostics.
    rejected_revision: Option<u64>,
    /// Linear RGBA16Float scene target.
    hdr: wgpu::Texture,
    /// Depth32Float scene target using zero-to-one projection depth.
    depth: wgpu::Texture,
    /// HDR texture and exposure-parameter layout.
    tone_layout: wgpu::BindGroupLayout,
    /// Exposure, explicit sRGB encoding flag, and alignment padding.
    tone_uniform: wgpu::Buffer,
    /// Tonemap bindings recreated whenever the HDR target changes.
    tone_group: wgpu::BindGroup,
    /// Whether the output attachment performs linear-to-sRGB conversion itself.
    srgb: bool,
}

impl PbrPipeline {
    /// Force the next render to reconsider cached asset uploads.
    pub fn invalidate_assets(&mut self) {
        self.assets.revision = u64::MAX;
    }

    /// Create the pass with the built-in mesh and tonemap shaders.
    pub fn new(
        d: &wgpu::Device,
        q: &wgpu::Queue,
        format: wgpu::TextureFormat,
        w: u32,
        h: u32,
    ) -> Self {
        Self::with_shaders(
            d,
            q,
            format,
            w,
            h,
            include_str!("shaders/pbr.wgsl"),
            include_str!("shaders/tonemap.wgsl"),
        )
    }

    /// Create a candidate pass using supplied WGSL entry points.
    ///
    /// The caller owns the wgpu validation error scope when loading external
    /// shaders. Keep the previous pass alive until the candidate passes validation.
    pub fn with_shaders(
        d: &wgpu::Device,
        q: &wgpu::Queue,
        format: wgpu::TextureFormat,
        w: u32,
        h: u32,
        pbr: &str,
        tone: &str,
    ) -> Self {
        // Step 1: establish the uniform and texture layouts shared by scene shaders.
        let uniform = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("frame uniforms"),
            size: 176,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let group = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform.as_entire_binding(),
            }],
        });
        let shader = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("PBR GGX"),
            source: wgpu::ShaderSource::Wgsl(pbr.into()),
        });
        let material_layout = crate::gpu_assets::material_layout(d);
        let environment_layout = crate::gpu_assets::environment_layout(d);
        let pipeline_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[&layout, &material_layout, &environment_layout],
            push_constant_ranges: &[],
        });
        // Step 2: create HDR mesh and background pipelines with compatible depth targets.
        let pipeline=d.create_render_pipeline(&wgpu::RenderPipelineDescriptor{
   label:Some("PBR opaque"),layout:Some(&pipeline_layout),vertex:wgpu::VertexState{module:&shader,entry_point:Some("vs_main"),compilation_options:Default::default(),buffers:&[
    wgpu::VertexBufferLayout{array_stride:56,step_mode:wgpu::VertexStepMode::Vertex,attributes:&[wgpu::VertexAttribute{format:wgpu::VertexFormat::Float32x3,offset:0,shader_location:0},wgpu::VertexAttribute{format:wgpu::VertexFormat::Float32x3,offset:12,shader_location:1},wgpu::VertexAttribute{format:wgpu::VertexFormat::Float32x2,offset:24,shader_location:2},wgpu::VertexAttribute{format:wgpu::VertexFormat::Float32x3,offset:32,shader_location:9},wgpu::VertexAttribute{format:wgpu::VertexFormat::Float32x3,offset:44,shader_location:10}]},
    wgpu::VertexBufferLayout{array_stride:96,step_mode:wgpu::VertexStepMode::Instance,attributes:&wgpu::vertex_attr_array![3=>Float32x4,4=>Float32x4,5=>Float32x4,6=>Float32x4,7=>Float32x4,8=>Float32x4]}]},
   fragment:Some(wgpu::FragmentState{module:&shader,entry_point:Some("fs_main"),compilation_options:Default::default(),targets:&[Some(wgpu::ColorTargetState{format:wgpu::TextureFormat::Rgba16Float,blend:None,write_mask:wgpu::ColorWrites::ALL})]}),
   primitive:wgpu::PrimitiveState{cull_mode:None,..Default::default()},depth_stencil:Some(wgpu::DepthStencilState{format:wgpu::TextureFormat::Depth32Float,depth_write_enabled:true,depth_compare:wgpu::CompareFunction::Less,stencil:Default::default(),bias:Default::default()}),multisample:Default::default(),multiview:None,cache:None});
        let background = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("environment background"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_background"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_background"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba16Float,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: Default::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: false,
                depth_compare: wgpu::CompareFunction::Always,
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            multiview: None,
            cache: None,
        });
        // Step 3: allocate neutral assets, fallback geometry, and the initial instance buffer.
        let assets = crate::gpu_assets::GpuAssets::new(
            d,
            q,
            &Default::default(),
            &material_layout,
            &environment_layout,
            [0; 4],
        );
        let (v, i) = sphere();
        let index_count = i.len() as u32;
        let vertices = buffer(
            d,
            "sphere vertices",
            bytemuck::cast_slice(&v),
            wgpu::BufferUsages::VERTEX,
        );
        let indices = buffer(
            d,
            "sphere indices",
            bytemuck::cast_slice(&i),
            wgpu::BufferUsages::INDEX,
        );
        let instances = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instances"),
            size: 96,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        // Step 4: create the exposure/tonemap pass and its size-dependent targets.
        let tone_layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let tone_uniform = buffer(
            d,
            "exposure",
            &[0; 16],
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        );
        let tone_shader = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("tonemap"),
            source: wgpu::ShaderSource::Wgsl(tone.into()),
        });
        let tone_pl = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[&tone_layout],
            push_constant_ranges: &[],
        });
        let tonemap = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tonemap"),
            layout: Some(&tone_pl),
            vertex: wgpu::VertexState {
                module: &tone_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &tone_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview: None,
            cache: None,
        });
        let hdr = target(d, w, h, wgpu::TextureFormat::Rgba16Float);
        let depth = target(d, w, h, wgpu::TextureFormat::Depth32Float);
        let tone_group = tone_group(d, &tone_layout, &tone_uniform, &hdr);
        Self {
            metrics: Default::default(),
            material_layout,
            environment_layout,
            assets,
            background,
            pipeline,
            tonemap,
            vertices,
            indices,
            index_count,
            uniform,
            group,
            instances,
            capacity: 1,
            staging: Vec::new(),
            missing: Default::default(),
            rejected_revision: None,
            hdr,
            depth,
            tone_layout,
            tone_uniform,
            tone_group,
            srgb: format.is_srgb(),
        }
    }

    /// Replace size-dependent HDR/depth targets and refresh tonemap bindings.
    pub fn resize(&mut self, d: &wgpu::Device, w: u32, h: u32) {
        self.hdr = target(d, w, h, wgpu::TextureFormat::Rgba16Float);
        self.depth = target(d, w, h, wgpu::TextureFormat::Depth32Float);
        self.tone_group = tone_group(d, &self.tone_layout, &self.tone_uniform, &self.hdr);
    }

    /// Prepare and submit the scene, background, and tonemap passes.
    ///
    /// Instances must already be sorted by material and mesh. The supplied viewport
    /// must fit both the output and intermediate targets; an empty rectangle emits
    /// only attachment clears. Missing cameras suppress geometry and background.
    pub fn render(
        &mut self,
        d: &wgpu::Device,
        q: &wgpu::Queue,
        output: &wgpu::TextureView,
        viewport: RenderViewport,
        f: &RenderFrame,
    ) {
        let started = std::time::Instant::now();
        // Step 1: accept a complete asset generation only if it fits the device limits.
        if self.assets.revision != f.assets.revision
            || self.assets.environment_ids != f.environment_ids
        {
            let limits = d.limits();
            let valid = f.assets.textures.iter().all(|(_, t)| {
                t.width <= limits.max_texture_dimension_2d
                    && t.height <= limits.max_texture_dimension_2d
            }) && f.assets.meshes.iter().all(|(_, m)| {
                m.vertices.len() as u64 * 56 <= limits.max_buffer_size
                    && m.indices.len() as u64 * 4 <= limits.max_buffer_size
            });
            if valid {
                self.assets = crate::gpu_assets::GpuAssets::new(
                    d,
                    q,
                    &f.assets,
                    &self.material_layout,
                    &self.environment_layout,
                    f.environment_ids,
                );
                self.rejected_revision = None;
            } else if self.rejected_revision != Some(f.assets.revision) {
                eprintln!("[render] asset generation exceeds device limits; retaining previous GPU assets");
                self.rejected_revision = Some(f.assets.revision);
            }
        }
        // Step 2: pack instances against the accepted CPU material snapshot.
        self.staging.clear();
        self.staging.extend(f.instances.iter().map(|i| {
            let m = self.assets.cpu.materials.get(&i.material.material);
            Instance {
                model: i.model,
                color: std::array::from_fn(|n| {
                    i.material.base_color[n] * m.map_or(1.0, |m| m.base_color[n])
                }),
                params: [
                    m.map_or(i.material.metallic, |m| m.metallic),
                    m.map_or(i.material.roughness, |m| m.roughness),
                    0.0,
                    0.0,
                ],
            }
        }));
        let data = &self.staging;
        if data.len() > self.capacity {
            self.capacity = data.len().next_power_of_two();
            self.instances = d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("instances"),
                size: (self.capacity * 96) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        if !data.is_empty() {
            q.write_buffer(&self.instances, 0, bytemuck::cast_slice(data));
        }
        // Step 3: upload camera, lighting, and exposure parameters for this packet.
        q.write_buffer(
            &self.uniform,
            0,
            bytemuck::bytes_of(&FrameUniform {
                vp: f.view_projection,
                eye: [
                    f.camera_position[0],
                    f.camera_position[1],
                    f.camera_position[2],
                    1.0,
                ],
                light_dir: [
                    f.light_direction[0],
                    f.light_direction[1],
                    f.light_direction[2],
                    0.0,
                ],
                light_color: [f.light_color[0], f.light_color[1], f.light_color[2], 0.0],
                inverse_vp: glam::Mat4::from_cols_array_2d(&f.view_projection)
                    .inverse()
                    .to_cols_array_2d(),
            }),
        );
        let tone = [f.exposure.to_bits(), u32::from(!self.srgb), 0, 0];
        q.write_buffer(&self.tone_uniform, 0, bytemuck::cast_slice(&tone));
        let hdr = self.hdr.create_view(&Default::default());
        let depth = self.depth.create_view(&Default::default());
        for i in &f.instances {
            let key = (i.material.mesh, i.material.material);
            if (key.0 != 0 && !self.assets.meshes.contains_key(&key.0)
                || key.1 != 0 && !self.assets.materials.contains_key(&key.1))
                && self.missing.insert(key)
            {
                eprintln!("[render] missing mesh/material {key:?}; using built-in fallback");
            }
        }
        self.metrics = crate::api::RenderMetrics {
            prepare_micros: started.elapsed().as_micros() as u64,
            instance_bytes: (data.len() * 96) as u64,
            ..Default::default()
        };
        // Step 4: encode the HDR scene, grouping consecutive material/mesh pairs.
        let encoding = std::time::Instant::now();
        let mut encoder = d.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("PBR"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &hdr,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.025,
                            g: 0.035,
                            b: 0.05,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            if viewport.width > 0 && viewport.height > 0 {
                pass.set_viewport(
                    viewport.x as f32,
                    viewport.y as f32,
                    viewport.width as f32,
                    viewport.height as f32,
                    0.0,
                    1.0,
                );
                pass.set_scissor_rect(viewport.x, viewport.y, viewport.width, viewport.height);
                if f.has_camera {
                    pass.set_pipeline(&self.background);
                    pass.set_bind_group(0, &self.group, &[]);
                    pass.set_bind_group(1, &self.assets.materials[&0], &[]);
                    pass.set_bind_group(2, &self.assets.environment, &[]);
                    if f.background {
                        pass.draw(0..3, 0..1);
                        self.metrics.draw_calls += 1;
                    }
                    pass.set_pipeline(&self.pipeline);
                    pass.set_bind_group(0, &self.group, &[]);
                    pass.set_vertex_buffer(0, self.vertices.slice(..));
                    pass.set_vertex_buffer(1, self.instances.slice(..));
                    pass.set_index_buffer(self.indices.slice(..), wgpu::IndexFormat::Uint32);
                    let mut start = 0;
                    while start < f.instances.len() {
                        let key = (
                            f.instances[start].material.mesh,
                            f.instances[start].material.material,
                        );
                        let mut end = start + 1;
                        while end < f.instances.len()
                            && (
                                f.instances[end].material.mesh,
                                f.instances[end].material.material,
                            ) == key
                        {
                            end += 1;
                        }
                        pass.set_bind_group(
                            1,
                            self.assets
                                .materials
                                .get(&key.1)
                                .unwrap_or(&self.assets.materials[&0]),
                            &[],
                        );
                        let count = if let Some(mesh) = self.assets.meshes.get(&key.0) {
                            pass.set_vertex_buffer(0, mesh.vertices.slice(..));
                            pass.set_index_buffer(
                                mesh.indices.slice(..),
                                wgpu::IndexFormat::Uint32,
                            );
                            mesh.count
                        } else {
                            pass.set_vertex_buffer(0, self.vertices.slice(..));
                            pass.set_index_buffer(
                                self.indices.slice(..),
                                wgpu::IndexFormat::Uint32,
                            );
                            self.index_count
                        };
                        self.metrics.draw_calls += 1;
                        pass.draw_indexed(0..count, 0, start as u32..end as u32);
                        start = end;
                    }
                }
            }
        }
        // Step 5: tonemap only the scene rectangle, leaving the rest transparent.
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("tonemap"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: output,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            if viewport.width > 0 && viewport.height > 0 {
                pass.set_viewport(
                    viewport.x as f32,
                    viewport.y as f32,
                    viewport.width as f32,
                    viewport.height as f32,
                    0.0,
                    1.0,
                );
                pass.set_scissor_rect(viewport.x, viewport.y, viewport.width, viewport.height);
                pass.set_pipeline(&self.tonemap);
                pass.set_bind_group(0, &self.tone_group, &[]);
                pass.draw(0..3, 0..1);
                self.metrics.draw_calls += 1;
            }
        }
        // Step 6: submit both passes without waiting for GPU completion.
        q.submit(Some(encoder.finish()));
        self.metrics.submit_micros = encoding.elapsed().as_micros() as u64;
    }
}

// =============================================================================
// Target Bindings and Fallback Geometry
// =============================================================================

/// Bind the current HDR target together with the persistent exposure buffer.
fn tone_group(
    d: &wgpu::Device,
    l: &wgpu::BindGroupLayout,
    u: &wgpu::Buffer,
    t: &wgpu::Texture,
) -> wgpu::BindGroup {
    d.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: l,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&t.create_view(&Default::default())),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: u.as_entire_binding(),
            },
        ],
    })
}

/// Generate the unit-radius UV sphere used for missing or zero mesh IDs.
fn sphere() -> (Vec<Vertex>, Vec<u32>) {
    let mut v = Vec::new();
    let mut i = Vec::new();
    let segments = 32;
    let rings = 16;
    for y in 0..=rings {
        let theta = y as f32 / rings as f32 * std::f32::consts::PI;
        for x in 0..=segments {
            let phi = x as f32 / segments as f32 * std::f32::consts::TAU;
            let p = [
                theta.sin() * phi.cos(),
                theta.cos(),
                theta.sin() * phi.sin(),
            ];
            v.push(Vertex {
                position: p,
                normal: p,
                uv: [x as f32 / segments as f32, y as f32 / rings as f32],
                tangent: [-phi.sin(), 0.0, phi.cos()],
                bitangent: [
                    theta.cos() * phi.cos(),
                    -theta.sin(),
                    theta.cos() * phi.sin(),
                ],
            });
        }
    }
    for y in 0..rings {
        for x in 0..segments {
            let a = y * (segments + 1) + x;
            let b = a + segments + 1;
            i.extend_from_slice(&[a, b, a + 1, a + 1, b, b + 1]);
        }
    }
    (v, i)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    /// Read RGBA8 pixels back to the CPU, stripping wgpu's 256-byte row padding.
    fn readback(d: &wgpu::Device, q: &wgpu::Queue, t: &wgpu::Texture) -> Vec<u8> {
        let w = t.width();
        let h = t.height();
        let stride = (w * 4).div_ceil(256) * 256;
        let b = d.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (stride * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut e = d.create_command_encoder(&Default::default());
        e.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: t,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &b,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        q.submit(Some(e.finish()));
        let (tx, rx) = std::sync::mpsc::channel();
        b.slice(..)
            .map_async(wgpu::MapMode::Read, move |r| tx.send(r).unwrap());
        d.poll(wgpu::PollType::Wait).unwrap();
        rx.recv().unwrap().unwrap();
        let data = b.slice(..).get_mapped_range();
        let pixels = data
            .chunks(stride as usize)
            .flat_map(|row| row[..w as usize * 4].iter().copied())
            .collect();
        drop(data);
        b.unmap();
        pixels
    }

    /// Exercise actual GPU shading, instancing, alpha masking, and shader rejection.
    ///
    /// Requires a native adapter; image assertions allow small backend differences.
    #[test]
    fn gpu_pbr_frame_validates() {
        let instance = wgpu::Instance::default();
        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .expect("native GPU acceptance test requires an adapter");
        eprintln!("PBR GPU: {:?}", adapter.get_info());
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).unwrap();
        let mut pipeline = PbrPipeline::new(
            &device,
            &queue,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            128,
            128,
        );
        let output = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: 128,
                height: 128,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let mut frame = RenderFrame::default();
        pipeline.render(
            &device,
            &queue,
            &output.create_view(&Default::default()),
            RenderViewport::full(128, 128),
            &frame,
        );
        let clear = readback(&device, &queue, &output);
        // Fixed-image baseline: ACES + one sRGB encoding, allowing backend rounding.
        let expected = [0.025f32, 0.035, 0.05].map(|x| {
            let mapped = (x * (2.51 * x + 0.03) / (x * (2.43 * x + 0.59) + 0.14)).clamp(0., 1.);
            ((if mapped < 0.0031308 {
                12.92 * mapped
            } else {
                1.055 * mapped.powf(1. / 2.4) - 0.055
            }) * 255.)
                .round() as u8
        });
        for pixel in clear.chunks_exact(4) {
            for c in 0..3 {
                assert!(
                    pixel[c].abs_diff(expected[c]) <= 3,
                    "gamma/tonemap mismatch {pixel:?} != {expected:?}"
                );
            }
            assert_eq!(pixel[3], 255);
        }
        frame.has_camera = true;
        frame.camera_position = [0., 0., 5.];
        frame.view_projection =
            (glam::camera::rh::proj::directx::perspective(60f32.to_radians(), 1., 0.1, 100.)
                * glam::Mat4::from_translation(glam::Vec3::new(0., 0., -5.)))
            .to_cols_array_2d();
        for y in 0..3 {
            for x in 0..3 {
                frame.instances.push(crate::RenderInstance {
                    model: glam::Mat4::from_scale_rotation_translation(
                        glam::Vec3::splat(0.6),
                        glam::Quat::IDENTITY,
                        glam::Vec3::new((x as f32 - 1.) * 1.4, (y as f32 - 1.) * 1.4, 0.),
                    )
                    .to_cols_array_2d(),
                    material: crate::PbrRenderableComponent {
                        base_color: [0.8, 0.18, 0.07, 1.],
                        metallic: x as f32 / 2.,
                        roughness: 0.1 + y as f32 * 0.4,
                        ..Default::default()
                    },
                });
            }
        }
        pipeline.render(
            &device,
            &queue,
            &output.create_view(&Default::default()),
            RenderViewport::full(128, 128),
            &frame,
        );
        let scene = readback(&device, &queue, &output);
        assert!(
            scene
                .iter()
                .zip(&clear)
                .filter(|(a, b)| a.abs_diff(**b) > 5)
                .count()
                > 1000
        );
        let center = &scene[(64 * 128 + 64) * 4..][..4];
        assert!(
            center[0] > center[2],
            "red material must remain red: {center:?}"
        );
        assert_eq!(pipeline.metrics.draw_calls, 3);
        eprintln!("PBR 9 instances, one mesh draw: {:?}", pipeline.metrics);
        #[cfg(feature = "asset-cooking")]
        {
            let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../../target/render-validation");
            std::fs::create_dir_all(&dir).unwrap();
            let mut png = png::Encoder::new(
                std::fs::File::create(dir.join("pbr-grid.png")).unwrap(),
                128,
                128,
            );
            png.set_color(png::ColorType::Rgba);
            png.set_depth(png::BitDepth::Eight);
            png.write_header()
                .unwrap()
                .write_image_data(&scene)
                .unwrap();
        }
        // A masked material must reproduce the empty frame within backend tolerance.
        frame.background = false;
        let mut assets = crate::assets::RenderAssets::default();
        assets.materials.insert(
            1,
            crate::assets::Material {
                base_color: [1., 1., 1., 0.],
                alpha_cutoff: 0.5,
                ..Default::default()
            },
        );
        assets.revision = 1;
        frame.assets = std::sync::Arc::new(assets);
        for instance in &mut frame.instances {
            instance.material.material = 1;
        }
        pipeline.render(
            &device,
            &queue,
            &output.create_view(&Default::default()),
            RenderViewport::full(128, 128),
            &frame,
        );
        let masked = readback(&device, &queue, &output);
        assert!(masked.iter().zip(&clear).all(|(a, b)| a.abs_diff(*b) <= 3));
        // Invalid replacement shaders are caught before the active pass is replaced.
        device.push_error_scope(wgpu::ErrorFilter::Validation);
        let _invalid = PbrPipeline::with_shaders(
            &device,
            &queue,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            128,
            128,
            "invalid WGSL",
            include_str!("shaders/tonemap.wgsl"),
        );
        assert!(pollster::block_on(device.pop_error_scope()).is_some());
        // A resized target, empty scene and clipped editor viewport remain valid.
        pipeline.resize(&device, 128, 128);
        frame.instances.clear();
        frame.has_camera = false;
        pipeline.render(
            &device,
            &queue,
            &output.create_view(&Default::default()),
            RenderViewport::new(10, 10, 80, 80),
            &frame,
        );
        device.poll(wgpu::PollType::Wait).unwrap();
    }
}

// =============================================================================
// Pass Interface
// =============================================================================

impl crate::graphics::Pass for PbrPipeline {
    fn label(&self) -> &str {
        "PBR opaque, environment and tonemap"
    }
    fn draw(&mut self, c: crate::graphics::GpuPassContext<'_>, frame: &RenderFrame) {
        self.render(c.device, c.queue, c.output, c.viewport, frame);
    }
}
