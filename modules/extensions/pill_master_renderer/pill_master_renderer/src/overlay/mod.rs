//! What the renderer draws over the chain's picture on every surface: debug
//! lines, gizmo handles, selection outlines, and the pick buffer - plus the
//! debug labels, which it lays out for the UI painter to draw.
//!
//! # Responsibilities
//!
//! - Build the overlay's pipelines once, beside the chain's ([`Overlay::new`]).
//! - Record a surface's overlay after its chain: the outline mask and edge,
//!   the depth-tested and on-top debug lines and the gizmos
//!   ([`Overlay::record`]).
//! - Lay the debug labels out as a `pill_draw` list over the bitmap font
//!   ([`label_list`], [`font::texture_set`]); the UI painter draws it, since
//!   a label is a screen-space textured quad like any UI.
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
//!
//! Lines, gizmo triangles and the outline are anti-aliased without
//! multisampling: each primitive is projected on the processor, widened in
//! screen space by a pixel of feather, and its fragment shader fades that
//! band back down to the edge the shape really has. Multisampling would cost
//! every surface of every frontend; this costs only the overlay's own
//! primitives.

// Standard library
use std::collections::HashSet;
use std::num::NonZeroU32;

// External crates
use pill_core::math::Vector2f;
use pill_draw::{Color, DrawList, Rect};
use pill_engine::{DebugFrame, Entity, LabelAnchor};
use pill_master_renderer_data::color_space::linear_to_srgb;
use pill_renderer_api::{PickHit, PickRequest, PickResult, PICK_HANDLE_BIT};

// Current crate
use crate::{
    assets::MeshVertex,
    components::RenderViewport,
    error::{capturing_validation, RendererError, Result},
    frame::RenderFrame,
    render_queue::{decompose_render_queue_key, RenderQueueItem},
    resources::{RendererCamera, RendererMeshHandle, RendererResourceStorage, Vertex},
    stream_buffer::StreamBuffer,
    Instance,
};

/// The bitmap font the debug labels are drawn in, and its painter texture.
pub(crate) mod font;
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
/// The distance from the camera, in view units, at which a primitive loses
/// its screen position and is clipped away.
const NEAR_CLIP_DEPTH: f32 = 1e-3;
/// The half width of a drawn overlay line, in pixels.
const LINE_HALF_WIDTH: f32 = 1.0;
/// How far past its half width a line's anti-aliasing fades, in pixels.
const LINE_FEATHER: f32 = 0.5;
/// Which side of its line each of a quad's six corners sits on, in order.
const LINE_QUAD_SIDES: [f32; 6] = [1.0, -1.0, 1.0, 1.0, -1.0, -1.0];
/// How far a gizmo triangle's corners are pushed out, in pixels, so its
/// anti-aliasing has a band to fade in.
const TRIANGLE_FEATHER: f32 = 1.0;
/// The smallest sine of a corner's half-angle the expansion trusts: sharper
/// corners would push the vertex out without bound.
const MINIMUM_MITER_SINE: f32 = 0.02;

/// A line vertex after projection: the corner of the line's quad, its colour,
/// and how many pixels from the line's centre it sits - negative on the other
/// side - which the fragment shader turns into coverage.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct LineVertex {
    clip: [f32; 4],
    color: [f32; 4],
    across: f32,
}

/// A gizmo-triangle vertex after projection: the corner pushed out by the
/// feather, its colour, and the barycentric corner it stands for, which the
/// fragment shader turns into edge coverage.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct TriangleVertex {
    clip: [f32; 4],
    color: [f32; 4],
    barycentric: [f32; 3],
}

/// A gizmo line vertex in the pick buffer: the corner of the line's quad and
/// its handle value.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct HandleLineVertex {
    clip: [f32; 4],
    value: u32,
}

/// A gizmo triangle vertex in the pick buffer: world position and handle
/// value.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct HandleVertex {
    position: [f32; 3],
    value: u32,
}

/// The outline uniform: colour, and the width in `settings[0]`.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct OutlineParameters {
    color: [f32; 4],
    settings: [f32; 4],
}

/// The grid uniform: the cell size in `cell[0]`, and the drawing region in
/// pixels in `viewport` - the fragment shader turns the pixel position back
/// into a clip-space ray through it.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct GridParameters {
    cell: [f32; 4],
    viewport: [f32; 4],
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
    outline_edge: wgpu::RenderPipeline,
    outline_mask: wgpu::RenderPipeline,
    mesh_pick: wgpu::RenderPipeline,
    handle_lines_pick: wgpu::RenderPipeline,
    handle_triangles_pick: wgpu::RenderPipeline,
    grid: wgpu::RenderPipeline,
    grid_parameters: wgpu::Buffer,
    grid_bind_group: wgpu::BindGroup,
    outline_layout: wgpu::BindGroupLayout,
    outline_parameters: wgpu::Buffer,
    tested_lines: StreamBuffer,
    on_top_lines_buffer: StreamBuffer,
    triangles_buffer: StreamBuffer,
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
        color_format: wgpu::TextureFormat,
        depth_format: wgpu::TextureFormat,
        camera_layout: &wgpu::BindGroupLayout,
    ) -> Result<Self> {
        capturing_validation(device, || {
            Self::create(device, color_format, depth_format, camera_layout)
        })
        .map_err(|detail| RendererError::Other {
            detail: format!("overlay pipelines: {detail}"),
        })
    }

    fn create(
        device: &wgpu::Device,
        color_format: wgpu::TextureFormat,
        depth_format: wgpu::TextureFormat,
        camera_layout: &wgpu::BindGroupLayout,
    ) -> Self {
        let world_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("overlay_world"),
            source: wgpu::ShaderSource::Wgsl(include_str!("world.wgsl").into()),
        });
        let outline_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("overlay_outline"),
            source: wgpu::ShaderSource::Wgsl(include_str!("outline.wgsl").into()),
        });
        let grid_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("overlay_grid"),
            source: wgpu::ShaderSource::Wgsl(include_str!("grid.wgsl").into()),
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

        // The grid's own uniform: the cell size and the drawing region, so
        // one fullscreen triangle can turn any pixel into a world ray.
        let grid_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("overlay_grid"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let grid_parameters = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("overlay_grid"),
            size: std::mem::size_of::<GridParameters>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let grid_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("overlay_grid"),
            layout: &grid_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: grid_parameters.as_entire_binding(),
            }],
        });

        let layout = |label: &str, groups: &[&wgpu::BindGroupLayout]| {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: &groups.iter().map(|group| Some(*group)).collect::<Vec<_>>(),
                immediate_size: 0,
            })
        };
        let camera_pipeline_layout = layout("overlay_camera", &[camera_layout]);
        let outline_pipeline_layout = layout("overlay_outline", &[&outline_layout]);
        let grid_pipeline_layout = layout("overlay_grid", &[camera_layout, &grid_layout]);

        let line_vertex = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<LineVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: attributes![0 => Float32x4, 1 => Float32x4, 2 => Float32],
        };
        let triangle_vertex = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<TriangleVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: attributes![0 => Float32x4, 1 => Float32x4, 2 => Float32x3],
        };
        let handle_vertex = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<HandleVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: attributes![0 => Float32x3, 1 => Uint32],
        };
        let handle_line_vertex = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<HandleLineVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: attributes![0 => Float32x4, 1 => Uint32],
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
        // The grid is pinned to the far plane, so its own compare keeps it
        // behind the chain's geometry and lets it show everywhere else.
        let grid_depth = depth_state(depth_format, wgpu::CompareFunction::LessEqual, false);
        let grid_target = blended.clone();

        let pipeline = |description: PipelineDescription| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(description.label),
                layout: Some(description.layout),
                vertex: wgpu::VertexState {
                    module: description.module,
                    entry_point: Some(description.vertex),
                    buffers: &description
                        .buffers
                        .iter()
                        .map(|buffer| Some((*buffer).clone()))
                        .collect::<Vec<_>>(),
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
                multiview_mask: None,
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
        let line_buffers = std::slice::from_ref(&line_vertex);
        let triangle_buffers = std::slice::from_ref(&triangle_vertex);
        let handle_buffers = std::slice::from_ref(&handle_vertex);
        let handle_line_buffers = std::slice::from_ref(&handle_line_vertex);
        Self {
            depth_tested_lines: pipeline(world(
                "overlay_lines_tested",
                "line_vertex",
                "line_fragment",
                line_buffers,
                wgpu::PrimitiveTopology::TriangleList,
                blended.clone(),
                Some(tested),
            )),
            on_top_lines: pipeline(world(
                "overlay_lines_on_top",
                "line_vertex",
                "line_fragment",
                line_buffers,
                wgpu::PrimitiveTopology::TriangleList,
                blended.clone(),
                Some(on_top.clone()),
            )),
            on_top_triangles: pipeline(world(
                "overlay_triangles_on_top",
                "triangle_vertex",
                "triangle_fragment",
                triangle_buffers,
                wgpu::PrimitiveTopology::TriangleList,
                blended.clone(),
                Some(on_top.clone()),
            )),
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
            grid: pipeline(PipelineDescription {
                label: "overlay_grid",
                layout: &grid_pipeline_layout,
                module: &grid_module,
                vertex: "grid_vertex",
                fragment: "grid_fragment",
                buffers: &[],
                topology: wgpu::PrimitiveTopology::TriangleList,
                target: grid_target,
                depth: Some(grid_depth),
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
                "handle_clip_vertex",
                "value_fragment",
                handle_line_buffers,
                wgpu::PrimitiveTopology::TriangleList,
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
            grid_parameters,
            grid_bind_group,
            outline_layout,
            outline_parameters,
            tested_lines: StreamBuffer::new("overlay_lines_tested", wgpu::BufferUsages::VERTEX),
            on_top_lines_buffer: StreamBuffer::new(
                "overlay_lines_on_top",
                wgpu::BufferUsages::VERTEX,
            ),
            triangles_buffer: StreamBuffer::new("overlay_triangles", wgpu::BufferUsages::VERTEX),
            handle_lines_buffer: StreamBuffer::new(
                "overlay_handle_lines",
                wgpu::BufferUsages::VERTEX,
            ),
            handle_triangles_buffer: StreamBuffer::new(
                "overlay_handle_triangles",
                wgpu::BufferUsages::VERTEX,
            ),
            targets: Vec::new(),
            readback: picking::PickReadback::default(),
        }
    }

    /// Record the overlay of one surface after its chain: outlines, debug
    /// lines and gizmos into the surface, and the pick buffer when
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
        // The scene grid, when the frame asked for one on this surface: the
        // editor's view-bound viewports draw it, a game's surface never does.
        let grid = frame
            .grid
            .as_ref()
            .filter(|grid| grid.surfaces.contains(&input.surface))
            .map(|grid| grid.cell_size);
        if frame.debug.is_empty() && outlined.is_empty() && picks.is_empty() && grid.is_none() {
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
        let view_projection = &input.camera.parameters_data.view_projection_matrix;
        let viewport_size = (
            input.viewport.width.max(1) as f32,
            input.viewport.height.max(1) as f32,
        );
        let (tested, on_top) = line_vertices(&frame.debug, view_projection, viewport_size);
        let triangles = triangle_vertices(&frame.debug, view_projection, viewport_size);
        let tested_count = tested.len() as u32;
        let on_top_count = on_top.len() as u32;
        let triangle_count = triangles.len() as u32;
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

        let draws_into_surface = !outlined.is_empty()
            || tested_buffer.is_some()
            || on_top_buffer.is_some()
            || triangles_buffer.is_some()
            || grid.is_some();
        if let Some(cell_size) = grid {
            let parameters = GridParameters {
                cell: [cell_size, 0.0, 0.0, 0.0],
                viewport: [
                    input.viewport.x as f32,
                    input.viewport.y as f32,
                    input.viewport.width.max(1) as f32,
                    input.viewport.height.max(1) as f32,
                ],
            };
            queue.write_buffer(&self.grid_parameters, 0, bytemuck::bytes_of(&parameters));
        }
        if draws_into_surface {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: input.color_view,
                    depth_slice: None,
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
                multiview_mask: None,
            });
            set_viewport(&mut pass, input.viewport);
            // The grid first: it is the scene's floor, and everything the
            // overlay draws above it belongs over it.
            if grid.is_some() {
                pass.set_pipeline(&self.grid);
                pass.set_bind_group(0, &input.camera.bind_group, &[]);
                pass.set_bind_group(1, &self.grid_bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
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
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
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
        let view_projection = &input.camera.parameters_data.view_projection_matrix;
        let viewport_size = (
            input.viewport.width.max(1) as f32,
            input.viewport.height.max(1) as f32,
        );
        let handle_lines = handle_line_vertices(&input.frame.debug, view_projection, viewport_size);
        let handle_triangles = handle_triangle_vertices(&input.frame.debug);
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
                    depth_slice: None,
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
                multiview_mask: None,
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
        depth_write_enabled: Some(write),
        depth_compare: Some(compare),
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

/// A vertex on its way to the screen: its clip-space position and the corner
/// of the shape it belongs to, carried through clipping.
type ClipVertex = ([f32; 4], [f32; 3]);

/// The frame's debug lines and gizmo lines as screen-space quads: those
/// tested against the scene's depth, and those drawn on top.
///
/// Each quad is the segment widened by the line's half width plus its
/// feather on each side, with every corner carrying its distance from the
/// line's centre, so the fragment shader can fade the edge. Widening happens
/// through the camera's own projection, which keeps a line the same width at
/// any depth.
fn line_vertices(
    debug: &DebugFrame,
    view_projection: &pill_core::math::Matrix4f,
    size: (f32, f32),
) -> (Vec<LineVertex>, Vec<LineVertex>) {
    let offset = LINE_HALF_WIDTH + LINE_FEATHER;
    let push_line =
        |output: &mut Vec<LineVertex>, start: [f32; 3], end: [f32; 3], color: [f32; 4]| {
            let Some(quad) = line_quad(start, end, view_projection, size, offset) else {
                return;
            };
            for (clip, side) in quad.into_iter().zip(LINE_QUAD_SIDES) {
                output.push(LineVertex {
                    clip,
                    color,
                    across: side * offset,
                });
            }
        };
    let mut tested = Vec::new();
    let mut on_top = Vec::new();
    for line in &debug.lines {
        if line.depth_test {
            push_line(&mut tested, line.start, line.end, line.color);
        } else {
            push_line(&mut on_top, line.start, line.end, line.color);
        }
    }
    for line in &debug.gizmo_lines {
        push_line(&mut on_top, line.start, line.end, line.color);
    }
    (tested, on_top)
}

/// The six clip-space corners of the quad covering `start`-`end`, widened by
/// `offset` pixels on each side, or `None` when the segment is behind the
/// camera or shorter than a pixel on screen.
fn line_quad(
    start: [f32; 3],
    end: [f32; 3],
    view_projection: &pill_core::math::Matrix4f,
    size: (f32, f32),
    offset: f32,
) -> Option<[[f32; 4]; 6]> {
    let project = |position: [f32; 3]| {
        (*view_projection * glam::Vec4::new(position[0], position[1], position[2], 1.0)).to_array()
    };
    let (start, end) = clip_segment_to_near(project(start), project(end))?;
    let (start_pixel, end_pixel) = (to_pixels(start, size), to_pixels(end, size));
    let along = [end_pixel.0 - start_pixel.0, end_pixel.1 - start_pixel.1];
    let length = (along[0] * along[0] + along[1] * along[1]).sqrt();
    if length < 1.0 {
        return None;
    }
    let normal = [-along[1] / length * offset, along[0] / length * offset];
    let corners = |clip: [f32; 4]| {
        let pixel = to_pixels(clip, size);
        [1.0, -1.0].map(|side| {
            from_pixels(
                (pixel.0 + normal[0] * side, pixel.1 + normal[1] * side),
                size,
                clip[2],
                clip[3],
            )
        })
    };
    let [start_plus, start_minus] = corners(start);
    let [end_plus, end_minus] = corners(end);
    Some([
        start_plus,
        start_minus,
        end_plus,
        end_plus,
        start_minus,
        end_minus,
    ])
}

/// A segment clipped to the camera's near side: `None` when both ends are
/// behind it, otherwise the part that crosses into view.
fn clip_segment_to_near(start: [f32; 4], end: [f32; 4]) -> Option<([f32; 4], [f32; 4])> {
    match (start[3] >= NEAR_CLIP_DEPTH, end[3] >= NEAR_CLIP_DEPTH) {
        (false, false) => None,
        (true, true) => Some((start, end)),
        (false, true) => Some((lerp4(start, end, crossing(start[3], end[3])), end)),
        (true, false) => Some((start, lerp4(end, start, crossing(end[3], start[3])))),
    }
}

/// A polygon clipped to the camera's near side, every vertex keeping the
/// barycentric corner it stands for.
fn clip_polygon_to_near(polygon: &[ClipVertex]) -> Vec<ClipVertex> {
    let mut clipped = Vec::with_capacity(polygon.len() + 1);
    for (index, current) in polygon.iter().enumerate() {
        let next = polygon[(index + 1) % polygon.len()];
        let current_inside = current.0[3] >= NEAR_CLIP_DEPTH;
        if current_inside {
            clipped.push(*current);
        }
        if current_inside != (next.0[3] >= NEAR_CLIP_DEPTH) {
            let t = crossing(current.0[3], next.0[3]);
            clipped.push((lerp4(current.0, next.0, t), lerp3(current.1, next.1, t)));
        }
    }
    clipped
}

/// How far along the way from a point at `start_depth` to one at `end_depth`
/// the camera's near side lies.
fn crossing(start_depth: f32, end_depth: f32) -> f32 {
    (NEAR_CLIP_DEPTH - start_depth) / (end_depth - start_depth)
}

/// Four components along the way from `start` to `end`.
fn lerp4(start: [f32; 4], end: [f32; 4], t: f32) -> [f32; 4] {
    std::array::from_fn(|index| start[index] + (end[index] - start[index]) * t)
}

/// Three components along the way from `start` to `end`.
fn lerp3(start: [f32; 3], end: [f32; 3], t: f32) -> [f32; 3] {
    std::array::from_fn(|index| start[index] + (end[index] - start[index]) * t)
}

/// A clip-space position in pixels from the centre of the drawn region.
fn to_pixels(clip: [f32; 4], size: (f32, f32)) -> (f32, f32) {
    let inverse = 1.0 / clip[3];
    (
        clip[0] * inverse * 0.5 * size.0,
        clip[1] * inverse * 0.5 * size.1,
    )
}

/// The clip-space position a pixel position maps back to at `depth`.
fn from_pixels(point: (f32, f32), size: (f32, f32), depth: f32, w: f32) -> [f32; 4] {
    [
        point.0 * 2.0 / size.0 * w,
        point.1 * 2.0 / size.1 * w,
        depth,
        w,
    ]
}

/// A direction of no length, or `None` when there is no direction at all.
fn unit(vector: [f32; 2]) -> Option<[f32; 2]> {
    let length = (vector[0] * vector[0] + vector[1] * vector[1]).sqrt();
    if length < 1e-6 {
        return None;
    }
    Some([vector[0] / length, vector[1] / length])
}

/// The frame's gizmo triangles as screen-space triangles whose corners are
/// pushed `TRIANGLE_FEATHER` pixels out, which the fragment shader feathers
/// back down to the silhouette the triangles really have.
fn triangle_vertices(
    debug: &DebugFrame,
    view_projection: &pill_core::math::Matrix4f,
    size: (f32, f32),
) -> Vec<TriangleVertex> {
    let mut vertices = Vec::new();
    for triangle in &debug.gizmo_triangles {
        let corners = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        let polygon: Vec<ClipVertex> = triangle
            .corners
            .iter()
            .zip(corners)
            .map(|(corner, barycentric)| {
                let clip = (*view_projection
                    * glam::Vec4::new(corner[0], corner[1], corner[2], 1.0))
                .to_array();
                (clip, barycentric)
            })
            .collect();
        let polygon = clip_polygon_to_near(&polygon);
        if polygon.len() < 3 {
            continue;
        }
        let expanded = expand_polygon(&polygon, size, TRIANGLE_FEATHER);
        for index in 1..expanded.len() - 1 {
            for (clip, barycentric) in [expanded[0], expanded[index], expanded[index + 1]] {
                vertices.push(TriangleVertex {
                    clip,
                    color: triangle.color,
                    barycentric,
                });
            }
        }
    }
    vertices
}

/// Push every vertex of a projected polygon `offset` pixels out from its two
/// edges: along the edge normal at a straight corner, farther at a sharp one
/// so both edges clear the original by `offset` and the feathered band has no
/// gaps at the joins.
fn expand_polygon(polygon: &[ClipVertex], size: (f32, f32), offset: f32) -> Vec<ClipVertex> {
    let points: Vec<(f32, f32)> = polygon
        .iter()
        .map(|vertex| to_pixels(vertex.0, size))
        .collect();
    let count = points.len();
    let double_area: f32 = (0..count)
        .map(|index| {
            let current = points[index];
            let next = points[(index + 1) % count];
            current.0 * next.1 - next.0 * current.1
        })
        .sum();
    let orientation = if double_area < 0.0 { -1.0 } else { 1.0 };
    let mut expanded = Vec::with_capacity(count);
    for index in 0..count {
        let current = points[index];
        let previous = points[(index + count - 1) % count];
        let next = points[(index + 1) % count];
        let incoming = unit([current.0 - previous.0, current.1 - previous.1]);
        let outgoing = unit([next.0 - current.0, next.1 - current.1]);
        let vertex = polygon[index];
        let moved = match (incoming, outgoing) {
            (Some(incoming), Some(outgoing)) => {
                let incoming_normal = [incoming[1] * orientation, -incoming[0] * orientation];
                let outgoing_normal = [outgoing[1] * orientation, -outgoing[0] * orientation];
                let turn = incoming[0] * outgoing[1] - incoming[1] * outgoing[0];
                let bisector = [
                    incoming_normal[0] + outgoing_normal[0],
                    incoming_normal[1] + outgoing_normal[1],
                ];
                match (turn * orientation > 0.0).then(|| unit(bisector)).flatten() {
                    Some(direction) => {
                        let along = (direction[0] * outgoing_normal[0]
                            + direction[1] * outgoing_normal[1])
                            .max(MINIMUM_MITER_SINE);
                        let reach = offset / along;
                        (
                            current.0 + direction[0] * reach,
                            current.1 + direction[1] * reach,
                        )
                    }
                    // A reflex corner of a clipped polygon sits on the cut,
                    // where nothing needs widening.
                    None => current,
                }
            }
            _ => current,
        };
        expanded.push((from_pixels(moved, size, vertex.0[2], vertex.0[3]), vertex.1));
    }
    expanded
}

/// The gizmo lines as the pick buffer draws them: the same quads as the drawn
/// lines, so picking covers the line the user sees.
fn handle_line_vertices(
    debug: &DebugFrame,
    view_projection: &pill_core::math::Matrix4f,
    size: (f32, f32),
) -> Vec<HandleLineVertex> {
    let mut vertices = Vec::new();
    for line in &debug.gizmo_lines {
        let value = PICK_HANDLE_BIT | line.handle;
        let Some(quad) = line_quad(line.start, line.end, view_projection, size, LINE_HALF_WIDTH)
        else {
            continue;
        };
        vertices.extend(quad.map(|clip| HandleLineVertex { clip, value }));
    }
    vertices
}

/// The frame's gizmo triangles as the pick buffer draws them, three
/// world-space vertices each.
fn handle_triangle_vertices(debug: &DebugFrame) -> Vec<HandleVertex> {
    debug
        .gizmo_triangles
        .iter()
        .flat_map(|triangle| {
            let value = PICK_HANDLE_BIT | triangle.handle;
            triangle
                .corners
                .map(|position| HandleVertex { position, value })
        })
        .collect()
}

/// The frame's labels as a `pill_draw` list in surface pixels: glyph quads
/// over the bitmap font texture, each over a shadow one glyph pixel down and
/// right. The UI painter draws it after the overlay, so text stays readable
/// over everything the overlay drew.
///
/// Debug colours are linear and the painter decodes sRGB vertex colours, so
/// each colour is encoded on the way in and lands unchanged.
pub(crate) fn label_list(
    debug: &DebugFrame,
    view_projection: &pill_core::math::Matrix4f,
    viewport: RenderViewport,
) -> DrawList {
    let mut list = DrawList::new();
    let (atlas_width, atlas_height) = font::atlas_size();
    let (atlas_width, atlas_height) = (atlas_width as f32, atlas_height as f32);
    for label in &debug.labels {
        let Some((x, y)) = label_origin(label.anchor, view_projection, viewport) else {
            continue;
        };
        for (offset, color) in [(font::GLYPH_SCALE, LABEL_SHADOW), (0.0, label.color)] {
            let color = Color::rgba(
                linear_to_srgb(color[0]),
                linear_to_srgb(color[1]),
                linear_to_srgb(color[2]),
                color[3],
            );
            for quad in font::layout_text(&label.text, x + offset, y + offset) {
                let rect = Rect::from_min_size(
                    Vector2f::new(quad.origin[0], quad.origin[1]),
                    Vector2f::new(quad.size[0], quad.size[1]),
                );
                let uv_min = Vector2f::new(
                    quad.atlas_origin[0] as f32 / atlas_width,
                    quad.atlas_origin[1] as f32 / atlas_height,
                );
                let uv_max = Vector2f::new(
                    (quad.atlas_origin[0] + font::GLYPH_WIDTH) as f32 / atlas_width,
                    (quad.atlas_origin[1] + font::GLYPH_HEIGHT) as f32 / atlas_height,
                );
                list.push_quad(font::FONT_TEXTURE, rect, uv_min, uv_max, color);
            }
        }
    }
    list
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
    use crate::ui::UiRenderer;

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

    /// Whatever the debug lines ask, gizmo lines draw on top; every line
    /// becomes a six-vertex quad, and the pick quad carries the handle.
    #[test]
    fn lines_split_by_depth_test() {
        let mut draw = pill_engine::DebugDraw::new(true);
        let style = pill_engine::DebugStyle::default();
        draw.line([-1.0, 0.0, -5.0], [1.0, 0.0, -5.0], style);
        draw.line(
            [-1.0, 0.0, -5.0],
            [1.0, 0.0, -5.0],
            pill_engine::DebugStyle {
                depth_test: false,
                ..style
            },
        );
        draw.gizmo_line([-1.0, 0.0, -5.0], [1.0, 0.0, -5.0], [1.0, 4.0, 1.0, 1.0], 5);
        let frame = draw.collect_frame(0.016);
        let projection = glam::camera::rh::proj::directx::perspective(1.0, 1.0, 0.1, 100.0);
        let (tested, on_top) = line_vertices(&frame, &projection, (100.0, 100.0));
        assert_eq!(tested.len(), 6 * usize::from(style.depth_test));
        assert_eq!(on_top.len(), 18 - tested.len());
        let handle_lines = handle_line_vertices(&frame, &projection, (100.0, 100.0));
        assert_eq!(handle_lines.len(), 6);
        assert_eq!(handle_lines[0].value, PICK_HANDLE_BIT | 5);
    }

    /// A line entirely behind the camera draws nothing; one that crosses the
    /// near side draws the part that is in view.
    #[test]
    fn lines_behind_the_camera_are_clipped_away() {
        let projection = glam::camera::rh::proj::directx::perspective(1.0, 1.0, 0.1, 100.0);
        let mut draw = pill_engine::DebugDraw::new(true);
        let style = pill_engine::DebugStyle::default();
        draw.line([-1.0, 0.0, 1.0], [1.0, 0.0, 2.0], style);
        let behind = draw.collect_frame(0.016);
        let (tested, on_top) = line_vertices(&behind, &projection, (100.0, 100.0));
        assert!(tested.is_empty() && on_top.is_empty());

        draw.line([-0.2, 2.0, 5.0], [1.0, -1.0, -5.0], style);
        let in_view = draw.collect_frame(0.016);
        let (tested, on_top) = line_vertices(&in_view, &projection, (100.0, 100.0));
        assert_eq!(tested.len() + on_top.len(), 6);
    }

    /// A gizmo triangle's corners move out past their projection, so the
    /// feather has a band to fade in, and keep their barycentric corners so
    /// the coverage still measures the true edge.
    #[test]
    fn gizmo_triangles_expand_for_antialiasing() {
        let projection = glam::camera::rh::proj::directx::perspective(1.0, 1.0, 0.1, 100.0);
        let mut draw = pill_engine::DebugDraw::new(true);
        let corners = [[-1.0, -1.0, -5.0], [1.0, -1.0, -5.0], [0.0, 1.0, -5.0]];
        draw.gizmo_triangle(corners, [1.0, 1.0, 1.0, 1.0], 7);
        let frame = draw.collect_frame(0.016);
        let vertices = triangle_vertices(&frame, &projection, (100.0, 100.0));
        assert_eq!(vertices.len(), 3);
        let mut barycentrics: Vec<[f32; 3]> =
            vertices.iter().map(|vertex| vertex.barycentric).collect();
        barycentrics.sort_by(|left, right| left.partial_cmp(right).unwrap());
        assert_eq!(
            barycentrics,
            vec![[0.0, 0.0, 1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]]
        );
        let clip = (projection * glam::Vec4::new(corners[0][0], corners[0][1], corners[0][2], 1.0))
            .to_array();
        let (raw_x, raw_y) = to_pixels(clip, (100.0, 100.0));
        let (moved_x, moved_y) = to_pixels(vertices[0].clip, (100.0, 100.0));
        assert!(
            moved_x < raw_x && moved_y < raw_y,
            "{moved_x} {moved_y} vs {raw_x} {raw_y}"
        );

        let mut behind = pill_engine::DebugDraw::new(true);
        behind.gizmo_triangle(
            [[-1.0, -1.0, 5.0], [1.0, -1.0, 5.0], [0.0, 1.0, 5.0]],
            [1.0, 1.0, 1.0, 1.0],
            7,
        );
        let behind = behind.collect_frame(0.016);
        assert!(triangle_vertices(&behind, &projection, (100.0, 100.0)).is_empty());
    }

    /// A device and queue without a surface, preferring the platform's
    /// software adapter (WARP on Windows) so the test runs without a GPU;
    /// `None` when the machine offers no adapter at all.
    async fn windowless_device() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            flags: wgpu::InstanceFlags::from_build_config(),
            backend_options: wgpu::BackendOptions::default(),
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: None,
                force_fallback_adapter: true,
                apply_limit_buckets: false,
            })
            .await
            .ok()?;
        adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("overlay label test device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::default(),
                trace: wgpu::Trace::default(),
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
            })
            .await
            .ok()
    }

    /// Draw `list` with `painter` into a transparent sRGB target of `size`
    /// and read its pixels back as tightly packed RGBA8 rows.
    fn render_pixels(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        painter: &mut UiRenderer,
        size: (u32, u32),
        list: &DrawList,
    ) -> Vec<u8> {
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("overlay label test target"),
            size: wgpu::Extent3d {
                width: size.0,
                height: size.1,
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
        // A texture-to-buffer copy pads every row to the copy alignment.
        let row_bytes = size.0 as usize * 4;
        let alignment = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize;
        let padded_row_bytes = row_bytes.div_ceil(alignment) * alignment;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("overlay label test readback"),
            size: (padded_row_bytes * size.1 as usize) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("overlay label test encoder"),
        });
        painter.render(device, queue, &mut encoder, &view, size, &[(list, 1.0)]);
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row_bytes as u32),
                    rows_per_image: Some(size.1),
                },
            },
            wgpu::Extent3d {
                width: size.0,
                height: size.1,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(Some(encoder.finish()));

        let slice = readback.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .expect("the readback finishes");
        receiver
            .recv()
            .expect("the map callback ran")
            .expect("the readback maps");
        let mapped = slice.get_mapped_range().expect("the mapped range");
        let mut pixels = Vec::with_capacity(row_bytes * size.1 as usize);
        for row in 0..size.1 as usize {
            let start = row * padded_row_bytes;
            pixels.extend_from_slice(&mapped[start..start + row_bytes]);
        }
        pixels
    }

    /// A debug label drawn through the UI painter lands in its linear debug
    /// colour - the bytes the label shader it replaced wrote into an sRGB
    /// target - only where its glyph is lit, over its shadow one glyph pixel
    /// down and right. Skipped on a machine with no adapter at all.
    #[test]
    fn labels_draw_through_the_painter_in_their_linear_colour() {
        let Some((device, queue)) = pill_core::platform::futures::block_on(windowless_device())
        else {
            eprintln!("no wgpu adapter on this machine; the label render test is skipped");
            return;
        };
        let mut painter = UiRenderer::new(
            &device,
            &queue,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            (64, 64),
        );
        painter.apply_texture_set(&device, &queue, &font::texture_set());

        // `|` lights its glyph's middle column, so the text covers x 4..6 and
        // y 0..14, and the shadow x 6..8 and y 2..16.
        let mut draw = pill_engine::DebugDraw::new(true);
        let style = pill_engine::DebugStyle::colored([0.5, 0.25, 1.0, 1.0]);
        draw.label_screen(0.0, 0.0, "|", style);
        let frame = draw.collect_frame(0.016);
        let list = label_list(
            &frame,
            &pill_core::math::Matrix4f::IDENTITY,
            RenderViewport::new(0, 0, 32, 32),
        );
        let size = (32u32, 32u32);
        let pixels = render_pixels(&device, &queue, &mut painter, size, &list);
        let pixel = |x: u32, y: u32| {
            let start = ((y * size.0 + x) * 4) as usize;
            [
                pixels[start],
                pixels[start + 1],
                pixels[start + 2],
                pixels[start + 3],
            ]
        };

        // Linear 0.5 and 0.25 encode to sRGB 188 and 137.
        let text = pixel(5, 1);
        let expected = [188, 137, 255, 255];
        for (channel, (got, want)) in text.iter().zip(expected).enumerate() {
            assert!(
                got.abs_diff(want) <= 1,
                "text channel {channel}: {text:?} vs {expected:?}"
            );
        }
        let shadow = pixel(7, 8);
        assert_eq!(&shadow[..3], &[0, 0, 0], "the shadow is black: {shadow:?}");
        assert!(
            shadow[3].abs_diff(217) <= 1,
            "the shadow is 85% opaque: {shadow:?}"
        );
        assert_eq!(pixel(20, 20), [0, 0, 0, 0], "nothing outside the label");
    }
}
