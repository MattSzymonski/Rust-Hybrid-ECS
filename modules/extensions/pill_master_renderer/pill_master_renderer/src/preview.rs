//! Asset previews: one small off-screen render, read back as pixels.
//!
//! # Responsibilities
//!
//! - Render one requested asset into a 128x128 RGBA8 target and copy it to a
//!   readback buffer ([`PreviewRenderer::record`],
//!   [`PreviewRenderer::record_mesh`]), at most one request per frame.
//! - Map that buffer after the frame that wrote it was submitted
//!   ([`PreviewRenderer::after_submit`]), and hand the pixels back once the
//!   GPU finished ([`PreviewRenderer::collect`]).
//! - Decode and upload a texture file the first time a preview names it, and
//!   again when the file changes ([`PreviewRenderer::texture`]); the same for
//!   a mesh file ([`PreviewRenderer::mesh`]).
//!
//! # Design
//!
//! A texture or material preview is one fullscreen triangle: the fragment
//! shader samples the asset's texture, or shades a sphere in a material's
//! colour, so no vertex buffers, camera or depth target are involved. A mesh
//! preview is a second pipeline that draws the decoded triangle list through a
//! camera fitted to the mesh's bounds, with a depth target because a solid
//! seen from a three-quarter angle occludes itself. The target and both
//! pipelines are built once and reused; the readback is a mapped buffer polled
//! without waiting, the shape the pick buffer already uses. Nothing is written
//! to disk: the pixels cross to the editor as data.
// Standard library
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

// External crates
use pill_renderer_api::PreviewImage;

/// Edge length of one preview, in texels.
pub const PREVIEW_SIZE: u32 = 128;

/// Bytes one preview's pixels take: RGBA8, tightly packed.
const PREVIEW_BYTES: u64 = (PREVIEW_SIZE as u64) * (PREVIEW_SIZE as u64) * 4;

/// How many GPU bytes the decoded texture files a preview drew may hold.
///
/// The cache exists so scrolling back to a tile costs no decode; it is not a
/// residency list, so it is bounded rather than grown for the session. A 4K
/// texture is 64 MB, so this holds one of those or a few dozen smaller ones.
const SOURCE_BUDGET_BYTES: u64 = 64 * 1024 * 1024;

/// How many GPU bytes the decoded mesh files may hold. A mesh is far smaller
/// than a texture, so the same budget covers a whole browsing session.
const MESH_BUDGET_BYTES: u64 = 64 * 1024 * 1024;

/// The mapping state of the readback, shared with the map callback.
const MAPPING_PENDING: u8 = 0;
const MAPPING_DONE: u8 = 1;
const MAPPING_FAILED: u8 = 2;

/// What the fragment shader draws, as the mode the parameters carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PreviewMode {
    /// A texture, fitted into the square.
    Texture = 0,
    /// A sphere in a material's colour.
    Material = 1,
}

/// The one uniform a flat preview writes.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct PreviewParameters {
    /// The material's colour; unused for a texture.
    color: [f32; 4],
    /// [`PreviewMode`] as the shader's `u32`, in the first word of a vec4 so
    /// the uniform stays a multiple of 16 bytes.
    mode: [u32; 4],
}

/// The uniform a mesh preview writes: the camera framing and the shading
/// colour.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct MeshParameters {
    /// Projection times view, with the mesh centred on the origin.
    transform: [[f32; 4]; 4],
    /// The grey a mesh preview shades in.
    color: [f32; 4],
}

/// One decoded mesh on the GPU, with the transform that frames it.
#[derive(Clone)]
pub(super) struct MeshSource {
    /// Interleaved vertices, in the asset crate's `MeshVertex` layout.
    vertices: wgpu::Buffer,
    /// Triangle indices into [`Self::vertices`].
    indices: wgpu::Buffer,
    /// How many indices to draw.
    index_count: u32,
    /// Projection times view, framing the mesh inside the square.
    transform: [[f32; 4]; 4],
}

/// The mesh vertex attributes the preview pipeline reads: position and
/// normal, at the offsets the asset crate's vertex struct puts them.
const MESH_ATTRIBUTES: [wgpu::VertexAttribute; 2] = [
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x3,
        offset: 0,
        shader_location: 0,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x3,
        offset: 20,
        shader_location: 1,
    },
];

/// One preview's target, pipelines and readback slot.
pub(super) struct PreviewRenderer {
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
    /// The mesh pipeline and its camera uniform: its own vertex layout,
    /// depth test and framing.
    mesh_layout: wgpu::BindGroupLayout,
    mesh_pipeline: wgpu::RenderPipeline,
    mesh_parameters: wgpu::Buffer,
    /// The depth target a mesh preview tests against.
    depth: wgpu::TextureView,
    /// The 128x128 render target every preview draws into, and its view.
    target: wgpu::Texture,
    view: wgpu::TextureView,
    /// Where the drawn pixels are copied for mapping.
    readback: wgpu::Buffer,
    /// The parameters buffer, rewritten per request.
    parameters: wgpu::Buffer,
    /// The sampler a texture preview reads through.
    sampler: wgpu::Sampler,
    /// A 1x1 white texture, bound for a material preview so the bind group is
    /// always the same shape.
    white: wgpu::TextureView,
    /// The texture files previews drew, by path, with the modified time each
    /// was decoded from.
    sources: HashMap<String, (Option<SystemTime>, wgpu::TextureView)>,
    /// The order the sources were last used in, oldest first, with the GPU
    /// bytes each holds: what the budget is enforced over.
    source_order: VecDeque<(String, u64)>,
    /// The GPU bytes `sources` holds.
    source_bytes: u64,
    /// The mesh files previews drew, by path, with the modified time each was
    /// decoded from.
    meshes: HashMap<String, (Option<SystemTime>, MeshSource)>,
    /// The order the meshes were last used in, oldest first, with the GPU
    /// bytes each holds.
    mesh_order: VecDeque<(String, u64)>,
    /// The GPU bytes `meshes` holds.
    mesh_bytes: u64,
    /// The request whose pixels are on their way, with its map state.
    in_flight: Option<(u64, Arc<AtomicU8>)>,
    /// Finished previews waiting for the next [`Self::collect`].
    finished: Vec<PreviewImage>,
}

impl PreviewRenderer {
    /// Build the target, pipeline and buffers once.
    pub(super) fn new(device: &wgpu::Device) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("preview_shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("preview.wgsl").into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("preview_bind_group_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("preview_pipeline_layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("preview_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("preview_vertex"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("preview_fragment"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8UnormSrgb,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("preview_target"),
            size: wgpu::Extent3d {
                width: PREVIEW_SIZE,
                height: PREVIEW_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("preview_readback"),
            size: PREVIEW_BYTES,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let parameters = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("preview_parameters"),
            size: std::mem::size_of::<PreviewParameters>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        // The mesh half of the pass: its own shader, camera uniform and depth
        // target, because a mesh needs perspective and occlusion where a quad
        // and a sphere need neither.
        let mesh_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("preview_mesh_shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("preview_mesh.wgsl").into()),
        });
        let mesh_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("preview_mesh_bind_group_layout"),
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
        let mesh_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("preview_mesh_pipeline_layout"),
            bind_group_layouts: &[Some(&mesh_layout)],
            immediate_size: 0,
        });
        let mesh_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("preview_mesh_pipeline"),
            layout: Some(&mesh_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &mesh_shader,
                entry_point: Some("preview_mesh_vertex"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<crate::assets::MeshVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &MESH_ATTRIBUTES,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &mesh_shader,
                entry_point: Some("preview_mesh_fragment"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8UnormSrgb,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                cull_mode: Some(wgpu::Face::Back),
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let mesh_parameters = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("preview_mesh_parameters"),
            size: std::mem::size_of::<MeshParameters>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let depth = device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("preview_depth"),
                size: wgpu::Extent3d {
                    width: PREVIEW_SIZE,
                    height: PREVIEW_SIZE,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Depth32Float,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            })
            .create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("preview_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        // A 1x1 white texture for the material mode's unused sampler slot.
        let white_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("preview_white"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let white = white_texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            layout,
            pipeline,
            mesh_layout,
            mesh_pipeline,
            mesh_parameters,
            depth,
            target,
            view,
            readback,
            parameters,
            sampler,
            white,
            sources: HashMap::new(),
            source_order: VecDeque::new(),
            source_bytes: 0,
            meshes: HashMap::new(),
            mesh_order: VecDeque::new(),
            mesh_bytes: 0,
            in_flight: None,
            finished: Vec::new(),
        }
    }

    /// Whether a request can be taken this frame: nothing is in flight and
    /// nothing is waiting to be handed over.
    pub(super) fn ready(&self) -> bool {
        self.in_flight.is_none() && self.finished.is_empty()
    }

    /// Answer a request this renderer will not draw, so the caller stops
    /// waiting for it.
    ///
    /// The refusal joins the finished list, which is what `collect` hands over,
    /// so a caller sees one answer per request it made.
    pub(super) fn refuse(&mut self, id: u64) {
        self.finished.push(PreviewImage::refused(id));
    }

    /// The source a texture preview samples: the file decoded and uploaded the
    /// first time, and again once its modified time moves, so a re-imported
    /// texture previews its new pixels.
    pub(super) fn texture(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        path: &str,
    ) -> Option<(wgpu::TextureView, wgpu::Sampler)> {
        let modified = std::fs::metadata(path)
            .ok()
            .and_then(|metadata| metadata.modified().ok());
        if let Some((cached, view)) = self.sources.get(path) {
            if *cached == modified {
                let view = view.clone();
                // Reused entries move to the newest end of the eviction queue:
                // the budget drops what a person scrolled past, not what they
                // are looking at.
                self.touch_source(path);
                return Some((view, self.sampler.clone()));
            }
        }
        let decoded = image::open(Path::new(path)).ok()?.to_rgba8();
        let size = wgpu::Extent3d {
            width: decoded.width(),
            height: decoded.height(),
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("preview_source"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            decoded.as_raw(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(decoded.width() * 4),
                rows_per_image: Some(decoded.height()),
            },
            size,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.sources
            .insert(path.to_string(), (modified, view.clone()));
        let bytes = u64::from(size.width) * u64::from(size.height) * 4;
        self.store_source(path, bytes);
        Some((view, self.sampler.clone()))
    }

    /// Record `path` as the newest source of `bytes`, then drop the oldest
    /// ones the budget can no longer hold.
    fn store_source(&mut self, path: &str, bytes: u64) {
        self.forget_source(path);
        self.source_order.push_back((path.to_string(), bytes));
        self.source_bytes += bytes;
        let dropped = trim_to_budget(
            &mut self.source_order,
            &mut self.source_bytes,
            SOURCE_BUDGET_BYTES,
        );
        for path in &dropped {
            self.sources.remove(path);
        }
        if !dropped.is_empty() {
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::RENDERING,
                dropped = dropped.len(),
                bytes = self.source_bytes,
                "[preview] source cache trimmed"
            );
        }
    }

    /// Move a source that was just used to the newest end of the eviction
    /// queue.
    fn touch_source(&mut self, path: &str) {
        if let Some(index) = self.source_order.iter().position(|(key, _)| key == path) {
            if let Some(entry) = self.source_order.remove(index) {
                self.source_order.push_back(entry);
            }
        }
    }

    /// Forget a source's accounting, for the entry an insert replaces.
    fn forget_source(&mut self, path: &str) {
        if let Some(index) = self.source_order.iter().position(|(key, _)| key == path) {
            if let Some((_, bytes)) = self.source_order.remove(index) {
                self.source_bytes = self.source_bytes.saturating_sub(bytes);
            }
        }
    }

    /// Record one preview into `encoder`: the asset's texture through
    /// `source`, or a sphere in `color` when it is `None`.
    ///
    /// The caller submits the encoder; [`Self::after_submit`] then starts the
    /// readback's mapping, and [`Self::collect`] hands the pixels over.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn record(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        id: u64,
        mode: PreviewMode,
        color: [f32; 4],
        source: Option<(wgpu::TextureView, wgpu::Sampler)>,
    ) {
        let (texture_view, sampler) = match source {
            Some((view, sampler)) => (view, sampler),
            None => (self.white.clone(), self.sampler.clone()),
        };
        let parameters = PreviewParameters {
            color,
            mode: [mode as u32, 0, 0, 0],
        };
        queue.write_buffer(&self.parameters, 0, bytemuck::bytes_of(&parameters));
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("preview_bind_group"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.parameters.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&texture_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("preview"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.copy_out(encoder);
        self.in_flight = Some((id, Arc::new(AtomicU8::new(MAPPING_PENDING))));
    }

    /// The mesh a preview draws: the file decoded and uploaded the first time,
    /// and again once its modified time moves.
    pub(super) fn mesh(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        path: &str,
    ) -> Option<MeshSource> {
        let modified = std::fs::metadata(path)
            .ok()
            .and_then(|metadata| metadata.modified().ok());
        if let Some((cached, mesh)) = self.meshes.get(path) {
            if *cached == modified {
                let mesh = mesh.clone();
                self.touch_mesh(path);
                return Some(mesh);
            }
        }
        let bytes = std::fs::read(Path::new(path)).ok()?;
        let decoded = crate::assets::Mesh::from_obj_bytes(path, &bytes).ok()?;
        if decoded.indices.is_empty() || decoded.vertices.is_empty() {
            return None;
        }
        let transform = framing(&decoded.vertices)?;
        let vertices = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("preview_mesh_vertices"),
            size: std::mem::size_of_val(decoded.vertices.as_slice()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&vertices, 0, bytemuck::cast_slice(&decoded.vertices));
        let indices = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("preview_mesh_indices"),
            size: std::mem::size_of_val(decoded.indices.as_slice()) as u64,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&indices, 0, bytemuck::cast_slice(&decoded.indices));
        let source = MeshSource {
            vertices,
            indices,
            index_count: decoded.indices.len() as u32,
            transform,
        };
        self.meshes
            .insert(path.to_string(), (modified, source.clone()));
        let bytes = source.vertices.size() + source.indices.size();
        self.store_mesh(path, bytes);
        Some(source)
    }

    /// Record `path` as the newest mesh source of `bytes`, then drop the oldest
    /// ones the budget can no longer hold.
    fn store_mesh(&mut self, path: &str, bytes: u64) {
        self.forget_mesh(path);
        self.mesh_order.push_back((path.to_string(), bytes));
        self.mesh_bytes += bytes;
        let dropped = trim_to_budget(
            &mut self.mesh_order,
            &mut self.mesh_bytes,
            MESH_BUDGET_BYTES,
        );
        for path in &dropped {
            self.meshes.remove(path);
        }
        if !dropped.is_empty() {
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::RENDERING,
                dropped = dropped.len(),
                bytes = self.mesh_bytes,
                "[preview] mesh cache trimmed"
            );
        }
    }

    /// Move a mesh that was just used to the newest end of the eviction queue.
    fn touch_mesh(&mut self, path: &str) {
        if let Some(index) = self.mesh_order.iter().position(|(key, _)| key == path) {
            if let Some(entry) = self.mesh_order.remove(index) {
                self.mesh_order.push_back(entry);
            }
        }
    }

    /// Forget a mesh's accounting, for the entry an insert replaces.
    fn forget_mesh(&mut self, path: &str) {
        if let Some(index) = self.mesh_order.iter().position(|(key, _)| key == path) {
            if let Some((_, bytes)) = self.mesh_order.remove(index) {
                self.mesh_bytes = self.mesh_bytes.saturating_sub(bytes);
            }
        }
    }

    /// Record one mesh preview into `encoder`, then copy the square out for
    /// mapping. The caller submits the encoder, as for the flat modes.
    pub(super) fn record_mesh(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        id: u64,
        mesh: &MeshSource,
    ) {
        let parameters = MeshParameters {
            transform: mesh.transform,
            color: [0.72, 0.73, 0.75, 1.0],
        };
        queue.write_buffer(&self.mesh_parameters, 0, bytemuck::bytes_of(&parameters));
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("preview_mesh_bind_group"),
            layout: &self.mesh_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: self.mesh_parameters.as_entire_binding(),
            }],
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("preview_mesh"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.mesh_pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.set_vertex_buffer(0, mesh.vertices.slice(..));
            pass.set_index_buffer(mesh.indices.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..mesh.index_count, 0, 0..1);
        }
        self.copy_out(encoder);
        self.in_flight = Some((id, Arc::new(AtomicU8::new(MAPPING_PENDING))));
    }

    /// Copy the target's pixels into the readback buffer, the step both
    /// preview paths end with.
    fn copy_out(&self, encoder: &mut wgpu::CommandEncoder) {
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    // RGBA8 at 128 texels is 512 bytes a row, already the
                    // copy alignment, so the pixels land tightly packed.
                    bytes_per_row: Some(PREVIEW_SIZE * 4),
                    rows_per_image: Some(PREVIEW_SIZE),
                },
            },
            wgpu::Extent3d {
                width: PREVIEW_SIZE,
                height: PREVIEW_SIZE,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Start mapping the readback the frame that recorded a preview wrote.
    ///
    /// Called after the submission: a buffer asked to map before its copy is
    /// submitted makes the submission fail.
    pub(super) fn after_submit(&mut self) {
        let Some((_, state)) = &self.in_flight else {
            return;
        };
        let reported = Arc::clone(state);
        self.readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let value = if result.is_ok() {
                    MAPPING_DONE
                } else {
                    MAPPING_FAILED
                };
                reported.store(value, Ordering::Release);
            });
    }

    /// Every preview the GPU finished, polling without waiting.
    pub(super) fn collect(&mut self, device: &wgpu::Device) -> Vec<PreviewImage> {
        if self.in_flight.is_some() {
            // Not waiting: an unfinished readback is answered on a later call.
            let _ = device.poll(wgpu::PollType::Poll);
        }
        let Some((id, state)) = &self.in_flight else {
            return std::mem::take(&mut self.finished);
        };
        match state.load(Ordering::Acquire) {
            MAPPING_DONE => match self.readback.slice(..).get_mapped_range() {
                Ok(pixels) => {
                    let rgba = pixels.to_vec();
                    drop(pixels);
                    self.readback.unmap();
                    self.finished.push(PreviewImage {
                        id: *id,
                        width: PREVIEW_SIZE,
                        height: PREVIEW_SIZE,
                        rgba,
                    });
                    self.in_flight = None;
                }
                Err(_) => {
                    self.readback.unmap();
                    self.in_flight = None;
                }
            },
            MAPPING_FAILED => {
                pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::RENDERING,
                    "[preview] the readback could not be mapped"
                );
                self.in_flight = None;
            }
            _ => {}
        }
        std::mem::take(&mut self.finished)
    }
}

/// The transform a mesh preview draws through: the mesh's own bounds decide
/// where the camera sits, so any asset fills the square.
///
/// The camera looks at the bounds' centre and backs off far enough for their
/// bounding sphere to fit the field of view, from a three-quarter angle so the
/// silhouette reads as a solid rather than a flat outline.
fn framing(vertices: &[crate::assets::MeshVertex]) -> Option<[[f32; 4]; 4]> {
    let mut min = glam::Vec3::splat(f32::INFINITY);
    let mut max = glam::Vec3::splat(f32::NEG_INFINITY);
    for vertex in vertices {
        let position = glam::Vec3::from_array(vertex.position);
        min = min.min(position);
        max = max.max(position);
    }
    if !min.is_finite() || !max.is_finite() || min == max {
        return None;
    }
    let centre = (min + max) * 0.5;
    let radius = ((max - min) * 0.5).length();
    let field_of_view = 45.0_f32.to_radians();
    let distance = radius / (field_of_view * 0.5).tan() * 1.15;
    let eye = centre + glam::Vec3::new(0.45, 0.35, 1.0).normalize() * distance;
    let view = glam::camera::rh::view::look_at_mat4(eye, centre, glam::Vec3::Y);
    // wgpu's clip space takes depth in 0..1 and Y up, which is the DirectX
    // convention glam names.
    let projection = glam::camera::rh::proj::directx::perspective(
        field_of_view,
        1.0,
        (distance - radius * 2.0).max(distance * 0.01),
        distance + radius * 4.0,
    );
    Some((projection * view).to_cols_array_2d())
}

/// The paths a cache must drop to fit `budget`, oldest first, with `total`
/// reduced by the bytes each carried.
///
/// The newest entry is never dropped, even when it alone is over the budget: a
/// source bigger than the whole cache still previews, it just leaves nothing
/// else resident.
fn trim_to_budget(
    order: &mut VecDeque<(String, u64)>,
    total: &mut u64,
    budget: u64,
) -> Vec<String> {
    let mut dropped = Vec::new();
    while *total > budget && order.len() > 1 {
        let Some((path, bytes)) = order.pop_front() else {
            break;
        };
        *total = total.saturating_sub(bytes);
        dropped.push(path);
    }
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A vertex at `position`, with the fields a preview never reads zeroed.
    fn vertex(position: [f32; 3]) -> crate::assets::MeshVertex {
        crate::assets::MeshVertex {
            position,
            texture_coordinates: [0.0, 0.0],
            normal: [0.0, 1.0, 0.0],
            tangent: [1.0, 0.0, 0.0],
            bitangent: [0.0, 0.0, 1.0],
        }
    }

    /// The corners of an axis-aligned box from `-1` to `1`.
    fn unit_box() -> Vec<crate::assets::MeshVertex> {
        let mut corners = Vec::with_capacity(8);
        for x in [-1.0, 1.0] {
            for y in [-1.0, 1.0] {
                for z in [-1.0, 1.0] {
                    corners.push(vertex([x, y, z]));
                }
            }
        }
        corners
    }

    #[test]
    fn a_mesh_preview_frames_the_whole_mesh_inside_the_square() {
        let matrix = glam::Mat4::from_cols_array_2d(
            &framing(&unit_box()).expect("a box has bounds to frame"),
        );
        let mut widest: f32 = 0.0;
        for corner in unit_box() {
            let clip = matrix
                * glam::Vec4::new(
                    corner.position[0],
                    corner.position[1],
                    corner.position[2],
                    1.0,
                );
            assert!(clip.w > 0.0, "a corner landed behind the camera");
            let ndc = clip.truncate() / clip.w;
            assert!(
                ndc.x.abs() <= 1.0 && ndc.y.abs() <= 1.0 && (0.0..=1.0).contains(&ndc.z),
                "a corner fell outside the clip volume: {ndc:?}"
            );
            widest = widest.max(ndc.x.abs().max(ndc.y.abs()));
        }
        assert!(
            widest >= 0.5,
            "the mesh filled only {widest:.2} of the square, so the preview would be mostly empty"
        );
    }

    #[test]
    fn a_mesh_with_no_extent_is_not_drawn() {
        assert!(framing(&[]).is_none());
        let point = vertex([0.5, 0.5, 0.5]);
        assert!(framing(&[point, point]).is_none());
        // A vertex the file left unplaced is no bound to frame: the rest of
        // the mesh is, and a mesh of nothing but unplaced vertices is not.
        assert!(framing(&[vertex([f32::NAN, 0.0, 0.0])]).is_none());
        assert!(framing(&[vertex([f32::NAN, 0.0, 0.0]), vertex([1.0, 1.0, 1.0])]).is_some());
    }

    /// A 4K texture is 64 MB, so browsing a folder of them must not keep every
    /// one resident.
    #[test]
    fn browsing_many_textures_keeps_the_cache_under_its_budget() {
        let texture_bytes = 4096 * 4096 * 4;
        let mut order = VecDeque::new();
        let mut total = 0;
        for index in 0..100u32 {
            let path = format!("textures/t{index}.png");
            order.push_back((path, texture_bytes));
            total += texture_bytes;
            let dropped = trim_to_budget(&mut order, &mut total, SOURCE_BUDGET_BYTES);
            // The cache drops as it grows, so what it holds never climbs past
            // the budget.
            assert!(
                total <= SOURCE_BUDGET_BYTES || order.len() == 1,
                "{} bytes held after {index} textures",
                total
            );
            assert_eq!(dropped.len(), usize::from(index > 0));
        }
        // The tile a person is on is the one that stays.
        assert_eq!(
            order.back().map(|(path, _)| path.as_str()),
            Some("textures/t99.png")
        );
        assert_eq!(order.len(), 1);
    }

    /// A source larger than the whole cache still previews: it is the newest
    /// entry, and the budget never drops the newest.
    #[test]
    fn a_source_larger_than_the_budget_is_still_kept() {
        let mut order = VecDeque::from([("huge.png".to_owned(), SOURCE_BUDGET_BYTES * 4)]);
        let mut total = SOURCE_BUDGET_BYTES * 4;

        let dropped = trim_to_budget(&mut order, &mut total, SOURCE_BUDGET_BYTES);

        assert!(dropped.is_empty());
        assert_eq!(order.len(), 1);
        assert_eq!(total, SOURCE_BUDGET_BYTES * 4);
    }
}
