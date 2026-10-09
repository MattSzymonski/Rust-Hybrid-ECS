//! What the renderer draws over the chain's picture on every surface: debug
//! lines and labels, gizmo handles, selection outlines, and the pick buffer.
//!
//! # Responsibilities
//!
//! - Build the overlay's pipelines once, beside the chain's ([`Overlay::new`]).
//! - Record a surface's overlay after its chain: the outline mask and edge,
//!   the depth-tested and on-top debug lines, the gizmos and the labels
//!   ([`Overlay::record`]).
//! - Draw the pick buffer for a surface that has picks waiting, and read the
//!   asked-for pixels back frames later ([`Overlay::take_results`]).
//!
//! # Design
//!
//! None of this goes through the chain: a project's chain decides how its
//! scene looks, and an outline or a debug line has to look the same whatever
//! the chain is. The overlay draws after the chain into the same surface view,
//! reading the depth buffer the chain's geometry passes left so a
//! depth-tested line hides behind what it is behind. The mask and pick passes
//! draw the frame's instances straight from the instance buffer the chain
//! already uploaded, so a queue position is an instance index in both.
//!
//! Every target here is sized to a surface. They are kept per size, so two
//! surfaces of different sizes do not rebuild them every frame.

// Standard library
use std::collections::HashSet;
use std::num::NonZeroU32;

// External crates
use pill_engine::{DebugFrame, Entity, LabelAnchor};
use pill_renderer_api::{PickHit, PickRequest, PickResult, PICK_HANDLE_BIT};
use wgpu::util::DeviceExt;

// Current crate
use crate::{
    assets::MeshVertex,
    components::RenderViewport,
    error::{capturing_validation, RendererError, Result},
    frame::RenderFrame,
    render_queue::{decompose_render_queue_key, RenderQueueItem},
    resources::{RendererCamera, RendererMeshHandle, RendererResourceStorage, Vertex},
    Instance,
};

mod font;
mod picking;

/// Width of a selection outline, in pixels.
const OUTLINE_WIDTH: f32 = 2.0;
/// Format of the outline mask.
const MASK_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;
/// Format of the pick buffer: one id per pixel.
const PICK_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R32Uint;
/// Format of the pick buffer's own depth.
const PICK_DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
/// Surface sizes whose targets are kept at once.
const KEPT_TARGET_SIZES: usize = 4;
/// The shadow under a label, so it reads over any background.
const LABEL_SHADOW: [f32; 4] = [0.0, 0.0, 0.0, 0.85];

/// A debug line or gizmo vertex: world position and colour.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct ColorVertex {
    position: [f32; 3],
    color: [f32; 4],
}

/// A gizmo vertex in the pick buffer: world position and handle value.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct HandleVertex {
    position: [f32; 3],
    value: u32,
}

/// A label quad corner: normalized device position, atlas coordinate, colour.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct LabelVertex {
    position: [f32; 2],
    texture_coordinates: [f32; 2],
    color: [f32; 4],
}

/// The outline uniform: colour, and the width in `settings[0]`.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct OutlineParameters {
    color: [f32; 4],
    settings: [f32; 4],
}

/// A vertex buffer rewritten every surface, grown when a frame outgrows it.
struct StreamBuffer {
    label: &'static str,
    buffer: Option<wgpu::Buffer>,
    capacity: u64,
}

impl StreamBuffer {
    fn new(label: &'static str) -> Self {
        Self {
            label,
            buffer: None,
            capacity: 0,
        }
    }

    /// Upload `bytes`; returns the buffer holding them, or `None` when empty.
    ///
    /// Each surface submits its own frame, and a queue write lands before the
    /// next submission, so one buffer serves every surface of a frame.
    fn write(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bytes: &[u8],
    ) -> Option<&wgpu::Buffer> {
        if bytes.is_empty() {
            return None;
        }
        let needed = bytes.len() as u64;
        if self.buffer.is_none() || needed > self.capacity {
            self.capacity = needed.next_power_of_two().max(4096);
            self.buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(self.label),
                size: self.capacity,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }
        let buffer = self.buffer.as_ref()?;
        queue.write_buffer(buffer, 0, bytes);
        Some(buffer)
    }
}

/// The targets sized to one surface size.
struct SizedTargets {
    width: u32,
    height: u32,
    /// The outlined meshes' silhouettes, and the edge pass reading it.
    mask_view: wgpu::TextureView,
    outline_bind_group: wgpu::BindGroup,
    /// The pick buffer and its depth, made the first time a pick asks.
    pick: Option<(wgpu::Texture, wgpu::TextureView, wgpu::TextureView)>,
}

/// One surface's frame, as the overlay needs it.
pub(crate) struct OverlayFrame<'a> {
    /// The surface being drawn.
    pub surface: u32,
    /// The view the chain drew into.
    pub color_view: &'a wgpu::TextureView,
    /// The depth the chain's geometry left.
    pub depth_view: &'a wgpu::TextureView,
    /// The surface size, in physical pixels.
    pub size: (u32, u32),
    /// The region the scene was drawn into.
    pub viewport: RenderViewport,
    /// The camera the surface was drawn through.
    pub camera: &'a RendererCamera,
    /// The frame.
    pub frame: &'a RenderFrame,
    /// The frame's queue: queue position is instance index.
    pub render_queue: &'a [RenderQueueItem],
    /// The instance buffer the chain drew from.
    pub instance_buffer: &'a wgpu::Buffer,
    /// The meshes the queue names.
    pub storage: &'a RendererResourceStorage,
}

/// The overlay's pipelines, buffers and targets.
pub(crate) struct Overlay {
    depth_tested_lines: wgpu::RenderPipeline,
    on_top_lines: wgpu::RenderPipeline,
    on_top_triangles: wgpu::RenderPipeline,
    labels: wgpu::RenderPipeline,
    outline_edge: wgpu::RenderPipeline,
    outline_mask: wgpu::RenderPipeline,
    mesh_pick: wgpu::RenderPipeline,
    handle_lines_pick: wgpu::RenderPipeline,
    handle_triangles_pick: wgpu::RenderPipeline,
    font_bind_group: wgpu::BindGroup,
    outline_layout: wgpu::BindGroupLayout,
    outline_parameters: wgpu::Buffer,
    tested_lines: StreamBuffer,
    on_top_lines_buffer: StreamBuffer,
    triangles_buffer: StreamBuffer,
    label_buffer: StreamBuffer,
    handle_lines_buffer: StreamBuffer,
    handle_triangles_buffer: StreamBuffer,
    targets: Vec<SizedTargets>,
    readback: picking::PickReadback,
}

/// A vertex attribute list for consecutive fields.
macro_rules! attributes {
    ($($location:expr => $format:ident),* $(,)?) => {
        &wgpu::vertex_attr_array![$($location => $format),*]
    };
}

impl Overlay {
    /// Build every overlay pipeline for a surface drawn in `color_format`
    /// whose geometry writes `depth_format`.
    ///
    /// # Errors
    ///
    /// Returns [`RendererError::Other`] when wgpu refuses a shader or pipeline.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        color_format: wgpu::TextureFormat,
        depth_format: wgpu::TextureFormat,
        camera_layout: &wgpu::BindGroupLayout,
    ) -> Result<Self> {
        capturing_validation(device, || {
            Self::create(device, queue, color_format, depth_format, camera_layout)
        })
        .map_err(|detail| RendererError::Other {
            detail: format!("overlay pipelines: {detail}"),
        })
    }

    fn create(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        color_format: wgpu::TextureFormat,
        depth_format: wgpu::TextureFormat,
        camera_layout: &wgpu::BindGroupLayout,
    ) -> Self {
        let world_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("overlay_world"),
            source: wgpu::ShaderSource::Wgsl(include_str!("world.wgsl").into()),
        });
        let label_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("overlay_label"),
            source: wgpu::ShaderSource::Wgsl(include_str!("label.wgsl").into()),
        });
        let outline_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("overlay_outline"),
            source: wgpu::ShaderSource::Wgsl(include_str!("outline.wgsl").into()),
        });

        // The font atlas and how labels sample it: nearest, so glyphs stay
        // crisp at any scale.
        let atlas = font::build_atlas();
        let font_texture = device.create_texture_with_data(
            queue,
            &wgpu::TextureDescriptor {
                label: Some("overlay_font"),
                size: wgpu::Extent3d {
                    width: atlas.width,
                    height: atlas.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            &atlas.texels,
        );
        let font_view = font_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let font_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("overlay_font"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let font_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("overlay_font"),
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
        let font_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("overlay_font"),
            layout: &font_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&font_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&font_sampler),
                },
            ],
        });

        // The outline edge pass reads the mask texel by texel.
        let outline_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("overlay_outline"),
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
        let outline_parameters = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("overlay_outline"),
            size: std::mem::size_of::<OutlineParameters>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let layout = |label: &str, groups: &[&wgpu::BindGroupLayout]| {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: groups,
                push_constant_ranges: &[],
            })
        };
        let camera_pipeline_layout = layout("overlay_camera", &[camera_layout]);
        let label_pipeline_layout = layout("overlay_label", &[&font_layout]);
        let outline_pipeline_layout = layout("overlay_outline", &[&outline_layout]);

        let color_vertex = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<ColorVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: attributes![0 => Float32x3, 1 => Float32x4],
        };
        let handle_vertex = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<HandleVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: attributes![0 => Float32x3, 1 => Uint32],
        };
        let label_vertex = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<LabelVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: attributes![0 => Float32x2, 1 => Float32x2, 2 => Float32x4],
        };
        // A mesh position, from the mesh's own vertex buffer, and the
        // instance's model rows from the chain's instance buffer.
        let mesh_position = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<MeshVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: attributes![0 => Float32x3],
        };
        let mesh_buffers = [mesh_position, Instance::data_layout_descriptor()];

        let blended = Some(wgpu::ColorTargetState {
            format: color_format,
            blend: Some(wgpu::BlendState::ALPHA_BLENDING),
            write_mask: wgpu::ColorWrites::ALL,
        });
        let value_target = Some(wgpu::ColorTargetState {
            format: PICK_FORMAT,
            blend: None,
            write_mask: wgpu::ColorWrites::ALL,
        });
        let mask_target = Some(wgpu::ColorTargetState {
            format: MASK_FORMAT,
            blend: None,
            write_mask: wgpu::ColorWrites::ALL,
        });
        let tested = depth_state(depth_format, wgpu::CompareFunction::LessEqual, false);
        let on_top = depth_state(depth_format, wgpu::CompareFunction::Always, false);
        let pick_on_top = depth_state(PICK_DEPTH_FORMAT, wgpu::CompareFunction::Always, false);
        let pick_mesh = depth_state(PICK_DEPTH_FORMAT, wgpu::CompareFunction::Less, true);

        let pipeline = |description: PipelineDescription| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(description.label),
                layout: Some(description.layout),
                vertex: wgpu::VertexState {
                    module: description.module,
                    entry_point: Some(description.vertex),
                    buffers: description.buffers,
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: description.module,
                    entry_point: Some(description.fragment),
                    targets: std::slice::from_ref(&description.target),
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology: description.topology,
                    ..Default::default()
                },
                depth_stencil: description.depth,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            })
        };
        let world =
            |label, vertex, fragment, buffers, topology, target, depth| PipelineDescription {
                label,
                layout: &camera_pipeline_layout,
                module: &world_module,
                vertex,
                fragment,
                buffers,
                topology,
                target,
                depth,
            };
        let color_buffers = std::slice::from_ref(&color_vertex);
        let handle_buffers = std::slice::from_ref(&handle_vertex);
        Self {
            depth_tested_lines: pipeline(world(
                "overlay_lines_tested",
                "color_vertex",
                "color_fragment",
                color_buffers,
                wgpu::PrimitiveTopology::LineList,
                blended.clone(),
                Some(tested),
            )),
            on_top_lines: pipeline(world(
                "overlay_lines_on_top",
                "color_vertex",
                "color_fragment",
                color_buffers,
                wgpu::PrimitiveTopology::LineList,
                blended.clone(),
                Some(on_top.clone()),
            )),
            on_top_triangles: pipeline(world(
                "overlay_triangles_on_top",
                "color_vertex",
                "color_fragment",
                color_buffers,
                wgpu::PrimitiveTopology::TriangleList,
                blended.clone(),
                Some(on_top.clone()),
            )),
            labels: pipeline(PipelineDescription {
                label: "overlay_labels",
                layout: &label_pipeline_layout,
                module: &label_module,
                vertex: "label_vertex",
                fragment: "label_fragment",
                buffers: std::slice::from_ref(&label_vertex),
                topology: wgpu::PrimitiveTopology::TriangleList,
                target: blended.clone(),
                depth: Some(on_top.clone()),
            }),
            outline_edge: pipeline(PipelineDescription {
                label: "overlay_outline_edge",
                layout: &outline_pipeline_layout,
                module: &outline_module,
                vertex: "fullscreen_vertex",
                fragment: "outline_fragment",
                buffers: &[],
                topology: wgpu::PrimitiveTopology::TriangleList,
                target: blended,
                depth: Some(on_top),
            }),
            outline_mask: pipeline(world(
                "overlay_outline_mask",
                "mesh_mask_vertex",
                "mask_fragment",
                &mesh_buffers[..],
                wgpu::PrimitiveTopology::TriangleList,
                mask_target,
                None,
            )),
            mesh_pick: pipeline(world(
                "overlay_pick_meshes",
                "mesh_pick_vertex",
                "value_fragment",
                &mesh_buffers[..],
                wgpu::PrimitiveTopology::TriangleList,
                value_target.clone(),
                Some(pick_mesh),
            )),
            handle_lines_pick: pipeline(world(
                "overlay_pick_handle_lines",
                "handle_vertex",
                "value_fragment",
                handle_buffers,
                wgpu::PrimitiveTopology::LineList,
                value_target.clone(),
                Some(pick_on_top.clone()),
            )),
            handle_triangles_pick: pipeline(world(
                "overlay_pick_handle_triangles",
                "handle_vertex",
                "value_fragment",
                handle_buffers,
                wgpu::PrimitiveTopology::TriangleList,
                value_target,
                Some(pick_on_top),
            )),
            font_bind_group,
            outline_layout,
            outline_parameters,
            tested_lines: StreamBuffer::new("overlay_lines_tested"),
            on_top_lines_buffer: StreamBuffer::new("overlay_lines_on_top"),
            triangles_buffer: StreamBuffer::new("overlay_triangles"),
            label_buffer: StreamBuffer::new("overlay_labels"),
            handle_lines_buffer: StreamBuffer::new("overlay_handle_lines"),
            handle_triangles_buffer: StreamBuffer::new("overlay_handle_triangles"),
            targets: Vec::new(),
            readback: picking::PickReadback::default(),
        }
    }

    /// Record the overlay of one surface after its chain: outlines, debug
    /// lines, gizmos and labels into the surface, and the pick buffer when
    /// picks on this surface are waiting.
    pub fn record(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        input: OverlayFrame,
    ) {
        let frame = input.frame;
        let picks: Vec<PickRequest> = frame
            .pick_requests
            .iter()
            .filter(|request| request.surface.0 == input.surface)
            .copied()
            .collect();
        let outlined = outlined_positions(frame, input.render_queue);
        if frame.debug.is_empty() && outlined.is_empty() && picks.is_empty() {
            return;
        }
        let target_index = self.ensure_targets(device, input.size);

        if !outlined.is_empty() {
            self.record_outline_mask(encoder, &input, target_index, &outlined);
            let parameters = OutlineParameters {
                color: frame.outline_color,
                settings: [OUTLINE_WIDTH, 0.0, 0.0, 0.0],
            };
            queue.write_buffer(&self.outline_parameters, 0, bytemuck::bytes_of(&parameters));
        }

        // Everything drawn into the surface, uploaded before the pass opens.
        let (tested, on_top) = line_vertices(&frame.debug);
        let triangles = gizmo_triangle_vertices(&frame.debug);
        let labels = label_vertices(&frame.debug, &input);
        let tested_count = tested.len() as u32;
        let on_top_count = on_top.len() as u32;
        let triangle_count = triangles.len() as u32;
        let label_count = labels.len() as u32;
        let tested_buffer = self
            .tested_lines
            .write(device, queue, bytemuck::cast_slice(&tested))
            .cloned();
        let on_top_buffer = self
            .on_top_lines_buffer
            .write(device, queue, bytemuck::cast_slice(&on_top))
            .cloned();
        let triangles_buffer = self
            .triangles_buffer
            .write(device, queue, bytemuck::cast_slice(&triangles))
            .cloned();
        let label_buffer = self
            .label_buffer
            .write(device, queue, bytemuck::cast_slice(&labels))
            .cloned();

        let draws_into_surface = !outlined.is_empty()
            || tested_buffer.is_some()
            || on_top_buffer.is_some()
            || triangles_buffer.is_some()
            || label_buffer.is_some();
        if draws_into_surface {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: input.color_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: input.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            set_viewport(&mut pass, input.viewport);
            // Outlines first, so a debug line drawn over a selected entity
            // stays visible.
            if !outlined.is_empty() {
                pass.set_pipeline(&self.outline_edge);
                pass.set_bind_group(0, &self.targets[target_index].outline_bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
            pass.set_bind_group(0, &input.camera.bind_group, &[]);
            let world_draws = [
                (&self.depth_tested_lines, &tested_buffer, tested_count),
                (&self.on_top_lines, &on_top_buffer, on_top_count),
                (&self.on_top_triangles, &triangles_buffer, triangle_count),
            ];
            for (pipeline, buffer, count) in world_draws {
                if let Some(buffer) = buffer {
                    pass.set_pipeline(pipeline);
                    pass.set_vertex_buffer(0, buffer.slice(..));
                    pass.draw(0..count, 0..1);
                }
            }
            // Labels last: text is what has to stay readable.
            if let Some(buffer) = &label_buffer {
                pass.set_pipeline(&self.labels);
                pass.set_bind_group(0, &self.font_bind_group, &[]);
                pass.set_vertex_buffer(0, buffer.slice(..));
                pass.draw(0..label_count, 0..1);
            }
        }

        if !picks.is_empty() {
            self.record_picks(device, queue, encoder, &input, target_index, picks);
        }
    }

    /// Start reading back the picks the frame just submitted recorded.
    pub fn after_submit(&mut self) {
        self.readback.start_mapping();
    }

    /// Answer, with nothing, every request whose surface was not drawn this
    /// frame: its pixel was never drawn, and it must not wait forever.
    pub fn answer_undrawn(&mut self, requests: &[PickRequest], drawn_surfaces: &[u32]) {
        for request in requests {
            if !drawn_surfaces.contains(&request.surface.0) {
                self.readback.answer(*request, PickHit::Nothing);
            }
        }
    }

    /// Every pick answer that has arrived since the last call.
    pub fn take_results(&mut self, device: &wgpu::Device) -> Vec<PickResult> {
        self.readback.collect(device)
    }

    /// The index of the targets sized `size`, creating them when missing.
    fn ensure_targets(&mut self, device: &wgpu::Device, (width, height): (u32, u32)) -> usize {
        let width = width.max(1);
        let height = height.max(1);
        if let Some(index) = self
            .targets
            .iter()
            .position(|targets| targets.width == width && targets.height == height)
        {
            return index;
        }
        // A resize leaves the old size behind; only the most recent few stay.
        if self.targets.len() >= KEPT_TARGET_SIZES {
            self.targets.remove(0);
        }
        let mask = create_target(
            device,
            "overlay_outline_mask",
            width,
            height,
            MASK_FORMAT,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        );
        let mask_view = mask.create_view(&wgpu::TextureViewDescriptor::default());
        let outline_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("overlay_outline"),
            layout: &self.outline_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&mask_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: self.outline_parameters.as_entire_binding(),
                },
            ],
        });
        self.targets.push(SizedTargets {
            width,
            height,
            mask_view,
            outline_bind_group,
            pick: None,
        });
        self.targets.len() - 1
    }

    /// Draw the outlined instances' silhouettes into the mask.
    fn record_outline_mask(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        input: &OverlayFrame,
        target_index: usize,
        outlined: &[u32],
    ) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("overlay_outline_mask"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &self.targets[target_index].mask_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        set_viewport(&mut pass, input.viewport);
        pass.set_pipeline(&self.outline_mask);
        pass.set_bind_group(0, &input.camera.bind_group, &[]);
        pass.set_vertex_buffer(1, input.instance_buffer.slice(..));
        draw_queue_positions(&mut pass, input, outlined.iter().copied());
    }

    /// Draw the pick buffer and record the copies of the requested pixels.
    fn record_picks(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        input: &OverlayFrame,
        target_index: usize,
        picks: Vec<PickRequest>,
    ) {
        let (width, height) = (
            self.targets[target_index].width,
            self.targets[target_index].height,
        );
        let pick = self.targets[target_index].pick.get_or_insert_with(|| {
            let texture = create_target(
                device,
                "overlay_pick",
                width,
                height,
                PICK_FORMAT,
                wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            );
            let depth = create_target(
                device,
                "overlay_pick_depth",
                width,
                height,
                PICK_DEPTH_FORMAT,
                wgpu::TextureUsages::RENDER_ATTACHMENT,
            );
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
            (texture, view, depth_view)
        });
        let (handle_lines, handle_triangles) = handle_vertices(&input.frame.debug);
        let handle_line_count = handle_lines.len() as u32;
        let handle_triangle_count = handle_triangles.len() as u32;
        let handle_lines_buffer = self
            .handle_lines_buffer
            .write(device, queue, bytemuck::cast_slice(&handle_lines))
            .cloned();
        let handle_triangles_buffer = self
            .handle_triangles_buffer
            .write(device, queue, bytemuck::cast_slice(&handle_triangles))
            .cloned();
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("overlay_pick"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &pick.1,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &pick.2,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            set_viewport(&mut pass, input.viewport);
            pass.set_bind_group(0, &input.camera.bind_group, &[]);
            pass.set_pipeline(&self.mesh_pick);
            pass.set_vertex_buffer(1, input.instance_buffer.slice(..));
            draw_queue_positions(&mut pass, input, 0..input.render_queue.len() as u32);
            // Handles over the meshes: a handle in front of an entity is what
            // a click on it means.
            let handle_draws = [
                (
                    &self.handle_lines_pick,
                    &handle_lines_buffer,
                    handle_line_count,
                ),
                (
                    &self.handle_triangles_pick,
                    &handle_triangles_buffer,
                    handle_triangle_count,
                ),
            ];
            for (pipeline, buffer, count) in handle_draws {
                if let Some(buffer) = buffer {
                    pass.set_pipeline(pipeline);
                    pass.set_vertex_buffer(0, buffer.slice(..));
                    pass.draw(0..count, 0..1);
                }
            }
        }
        let entities: Vec<Entity> = input
            .render_queue
            .iter()
            .map(|item| input.frame.instances[item.entity_index as usize].entity)
            .collect();
        self.readback
            .copy_requested_pixels(device, encoder, &pick.0, picks, entities);
    }
}

/// The settings one overlay pipeline differs in.
struct PipelineDescription<'a> {
    label: &'a str,
    layout: &'a wgpu::PipelineLayout,
    module: &'a wgpu::ShaderModule,
    vertex: &'a str,
    fragment: &'a str,
    buffers: &'a [wgpu::VertexBufferLayout<'a>],
    topology: wgpu::PrimitiveTopology,
    target: Option<wgpu::ColorTargetState>,
    depth: Option<wgpu::DepthStencilState>,
}

/// A depth state that reads `format` with `compare`, writing when `write`.
fn depth_state(
    format: wgpu::TextureFormat,
    compare: wgpu::CompareFunction,
    write: bool,
) -> wgpu::DepthStencilState {
    wgpu::DepthStencilState {
        format,
        depth_write_enabled: write,
        depth_compare: compare,
        stencil: wgpu::StencilState::default(),
        bias: wgpu::DepthBiasState::default(),
    }
}

/// A single-sample 2D texture sized `width` by `height`.
fn create_target(
    device: &wgpu::Device,
    label: &str,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    usage: wgpu::TextureUsages,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    })
}

/// Clip a pass to the region the scene was drawn into.
fn set_viewport(pass: &mut wgpu::RenderPass, viewport: RenderViewport) {
    pass.set_viewport(
        viewport.x as f32,
        viewport.y as f32,
        viewport.width as f32,
        viewport.height as f32,
        0.0,
        1.0,
    );
    pass.set_scissor_rect(viewport.x, viewport.y, viewport.width, viewport.height);
}

/// The queue positions of the instances whose entity is outlined.
fn outlined_positions(frame: &RenderFrame, render_queue: &[RenderQueueItem]) -> Vec<u32> {
    if frame.outlined.is_empty() {
        return Vec::new();
    }
    let outlined: HashSet<Entity> = frame.outlined.iter().copied().collect();
    render_queue
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            frame
                .instances
                .get(item.entity_index as usize)
                .is_some_and(|instance| outlined.contains(&instance.entity))
        })
        .map(|(position, _)| position as u32)
        .collect()
}

/// Draw each queue position's mesh as the instance at that position; runs of
/// consecutive positions sharing a mesh become one draw.
fn draw_queue_positions(
    pass: &mut wgpu::RenderPass,
    input: &OverlayFrame,
    positions: impl Iterator<Item = u32>,
) {
    let mut bound_mesh: Option<(RendererMeshHandle, u32)> = None;
    let mut run: Option<std::ops::Range<u32>> = None;
    for position in positions {
        let Some(item) = input.render_queue.get(position as usize) else {
            continue;
        };
        let fields = decompose_render_queue_key(item.key);
        let Some(version) = NonZeroU32::new(fields.mesh_version.into()) else {
            continue;
        };
        let handle = RendererMeshHandle::new(fields.mesh_index.into(), version);
        let continues = bound_mesh.is_some_and(|(bound, _)| bound == handle)
            && run.as_ref().is_some_and(|run| run.end == position);
        if continues {
            if let Some(run) = &mut run {
                run.end += 1;
            }
            continue;
        }
        if let (Some((_, index_count)), Some(run)) = (bound_mesh, run.take()) {
            pass.draw_indexed(0..index_count, 0, run);
        }
        if bound_mesh.is_none_or(|(bound, _)| bound != handle) {
            let Some(mesh) = input.storage.meshes.get(handle) else {
                bound_mesh = None;
                continue;
            };
            pass.set_vertex_buffer(0, mesh.vertex_buffer.slice(..));
            pass.set_index_buffer(mesh.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            bound_mesh = Some((handle, mesh.index_count));
        }
        run = Some(position..position + 1);
    }
    if let (Some((_, index_count)), Some(run)) = (bound_mesh, run) {
        pass.draw_indexed(0..index_count, 0, run);
    }
}

/// The frame's debug lines and gizmo lines as vertex pairs: those tested
/// against the scene's depth, and those drawn on top.
fn line_vertices(debug: &DebugFrame) -> (Vec<ColorVertex>, Vec<ColorVertex>) {
    let mut tested = Vec::new();
    let mut on_top = Vec::new();
    for line in &debug.lines {
        let output = if line.depth_test {
            &mut tested
        } else {
            &mut on_top
        };
        output.push(ColorVertex {
            position: line.start,
            color: line.color,
        });
        output.push(ColorVertex {
            position: line.end,
            color: line.color,
        });
    }
    for line in &debug.gizmo_lines {
        on_top.push(ColorVertex {
            position: line.start,
            color: line.color,
        });
        on_top.push(ColorVertex {
            position: line.end,
            color: line.color,
        });
    }
    (tested, on_top)
}

/// The frame's gizmo triangles, three vertices each.
fn gizmo_triangle_vertices(debug: &DebugFrame) -> Vec<ColorVertex> {
    debug
        .gizmo_triangles
        .iter()
        .flat_map(|triangle| {
            triangle.corners.map(|position| ColorVertex {
                position,
                color: triangle.color,
            })
        })
        .collect()
}

/// The gizmo lines and triangles as the pick buffer draws them.
fn handle_vertices(debug: &DebugFrame) -> (Vec<HandleVertex>, Vec<HandleVertex>) {
    let lines = debug
        .gizmo_lines
        .iter()
        .flat_map(|line| {
            let value = PICK_HANDLE_BIT | line.handle;
            [line.start, line.end].map(|position| HandleVertex { position, value })
        })
        .collect();
    let triangles = debug
        .gizmo_triangles
        .iter()
        .flat_map(|triangle| {
            let value = PICK_HANDLE_BIT | triangle.handle;
            triangle
                .corners
                .map(|position| HandleVertex { position, value })
        })
        .collect();
    (lines, triangles)
}

/// The frame's labels as glyph quads in normalized device coordinates, each
/// over a shadow one pixel down and right.
fn label_vertices(debug: &DebugFrame, input: &OverlayFrame) -> Vec<LabelVertex> {
    let (width, height) = (input.size.0.max(1) as f32, input.size.1.max(1) as f32);
    let atlas = (font_atlas_size().0 as f32, font_atlas_size().1 as f32);
    let mut vertices = Vec::new();
    for label in &debug.labels {
        let Some((x, y)) = label_origin(
            label.anchor,
            &input.camera.parameters_data.view_projection_matrix,
            input.viewport,
        ) else {
            continue;
        };
        for (offset, color) in [(font::GLYPH_SCALE, LABEL_SHADOW), (0.0, label.color)] {
            for quad in font::layout_text(&label.text, x + offset, y + offset) {
                let left = quad.origin[0] / width * 2.0 - 1.0;
                let right = (quad.origin[0] + quad.size[0]) / width * 2.0 - 1.0;
                let top = 1.0 - quad.origin[1] / height * 2.0;
                let bottom = 1.0 - (quad.origin[1] + quad.size[1]) / height * 2.0;
                let u0 = quad.atlas_origin[0] as f32 / atlas.0;
                let v0 = quad.atlas_origin[1] as f32 / atlas.1;
                let u1 = (quad.atlas_origin[0] + font::GLYPH_WIDTH) as f32 / atlas.0;
                let v1 = (quad.atlas_origin[1] + font::GLYPH_HEIGHT) as f32 / atlas.1;
                let corner = |x, y, u, v| LabelVertex {
                    position: [x, y],
                    texture_coordinates: [u, v],
                    color,
                };
                vertices.extend([
                    corner(left, top, u0, v0),
                    corner(left, bottom, u0, v1),
                    corner(right, top, u1, v0),
                    corner(right, top, u1, v0),
                    corner(left, bottom, u0, v1),
                    corner(right, bottom, u1, v1),
                ]);
            }
        }
    }
    vertices
}

/// The font atlas's size in texels.
fn font_atlas_size() -> (u32, u32) {
    static SIZE: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
    *SIZE.get_or_init(|| {
        let atlas = font::build_atlas();
        (atlas.width, atlas.height)
    })
}

/// Where a label's text starts, in surface pixels: a screen anchor as given,
/// a world anchor projected through the camera into the viewport. `None` for
/// a world point behind the camera.
fn label_origin(
    anchor: LabelAnchor,
    view_projection: &pill_core::math::Matrix4f,
    viewport: RenderViewport,
) -> Option<(f32, f32)> {
    match anchor {
        LabelAnchor::Screen { x, y } => Some((x, y)),
        LabelAnchor::World(position) => {
            let clip =
                *view_projection * glam::Vec4::new(position[0], position[1], position[2], 1.0);
            if clip.w <= f32::EPSILON {
                return None;
            }
            let normalized_x = clip.x / clip.w;
            let normalized_y = clip.y / clip.w;
            Some((
                viewport.x as f32 + (normalized_x * 0.5 + 0.5) * viewport.width as f32,
                viewport.y as f32 + (0.5 - normalized_y * 0.5) * viewport.height as f32,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A world point straight ahead lands in the middle of the viewport, and
    /// one behind the camera is not drawn.
    #[test]
    fn world_labels_project_into_the_viewport() {
        let projection = glam::camera::rh::proj::directx::perspective(1.0, 2.0, 0.1, 100.0);
        let viewport = RenderViewport::new(100, 50, 400, 200);
        let (x, y) = label_origin(LabelAnchor::World([0.0, 0.0, -5.0]), &projection, viewport)
            .expect("in front of the camera");
        assert!((x - 300.0).abs() < 1e-3, "{x}");
        assert!((y - 150.0).abs() < 1e-3, "{y}");
        assert!(label_origin(LabelAnchor::World([0.0, 0.0, 5.0]), &projection, viewport).is_none());
        assert_eq!(
            label_origin(
                LabelAnchor::Screen { x: 4.0, y: 8.0 },
                &projection,
                viewport
            ),
            Some((4.0, 8.0))
        );
    }

    /// Gizmo lines draw on top whatever the debug lines ask, and each line is
    /// two vertices.
    #[test]
    fn lines_split_by_depth_test() {
        let mut draw = pill_engine::DebugDraw::new(true);
        let style = pill_engine::DebugStyle::default();
        draw.line([0.0; 3], [1.0; 3], style);
        draw.line(
            [0.0; 3],
            [1.0; 3],
            pill_engine::DebugStyle {
                depth_test: false,
                ..style
            },
        );
        draw.gizmo_line([0.0; 3], [1.0; 3], [1.0; 4], 5);
        let frame = draw.collect_frame(0.016);
        let (tested, on_top) = line_vertices(&frame);
        assert_eq!(tested.len(), 2 * usize::from(style.depth_test));
        assert_eq!(on_top.len(), 6 - tested.len());
        let (handle_lines, _) = handle_vertices(&frame);
        assert_eq!(handle_lines[0].value, PICK_HANDLE_BIT | 5);
    }
}
