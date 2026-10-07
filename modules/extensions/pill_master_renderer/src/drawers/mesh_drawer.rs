//! Batching and draw recording for the master renderer's mesh path.
//!
//! # Responsibilities
//!
//! - Track the shader, material, and mesh the bound GPU resources belong to,
//!   flushing the accumulated instances when any of them changes
//!   ([`DrawingContext`]).
//! - Own the instance buffer, keep it large enough for a frame's instances,
//!   and upload them once per frame ([`MeshDrawer::upload_instances`]).
//! - Open a pass's render pass and record every draw its queue ranges imply
//!   inside it ([`MeshDrawer::record_draw_commands`]).
//!
//! # Design
//!
//! The queue arrives sorted by its packed key - draw order first, then shader,
//! material, and mesh - so items that agree on all three state handles are
//! drawn as one instanced call, and draws are recorded only where the state
//! changes or a range ends. The frame's instances are uploaded once, in queue
//! order, into one growable buffer, so a queue position is also an instance
//! index: every pass draws straight from that upload, and nothing is copied
//! or written per pass.

// Standard library
use std::{num::NonZeroU32, ops::Range};

// External crates
use pill_core::{debug, PillStyle};

// Current crate
use crate::error::Result;
use crate::{
    components::RenderViewport,
    config::{
        CAMERA_PARAMETERS_BIND_GROUP_LAYOUT_INDEX, ENGINE_PARAMETERS_BIND_GROUP_LAYOUT_INDEX,
        MATERIAL_PARAMETERS_BIND_GROUP_LAYOUT_INDEX, MATERIAL_TEXTURES_BIND_GROUP_LAYOUT_INDEX,
    },
    profiler::Profiler,
    render_queue::{decompose_render_queue_key, RenderQueueItem},
    resources::{
        RendererCamera, RendererMaterialHandle, RendererMeshHandle, RendererResourceStorage,
        RendererShader, RendererShaderHandle,
    },
    Instance,
};

/// Accumulated draw state for the run of instances being recorded.
///
/// Holds the shader, material, and mesh the bound GPU resources belong to,
/// plus the instance range accumulated since the last recorded draw. Starting
/// empty through `Default` makes the first item count as a change, so its
/// resources are bound before anything is drawn.
#[derive(Debug, Clone, Default)]
pub(crate) struct DrawingContext {
    shader_handle: Option<RendererShaderHandle>,
    shader_name: String,
    material_handle: Option<RendererMaterialHandle>,
    material_name: String,
    mesh_handle: Option<RendererMeshHandle>,
    mesh_name: String,
    mesh_index_count: u32, // Number of indices in the current mesh

    accumulated_instance_range: Range<u32>,
    accumulated_instance_count: u32,
}

impl DrawingContext {
    /// Emits the telemetry line describing the draw just recorded.
    ///
    /// Reports the instance range and the shader, material, and mesh names,
    /// which is what makes a mis-batched frame traceable from the log alone.
    pub fn log(&self) {
        debug!(
            target: pill_core::telemetry::telemetry_target::RENDERING,
            "Draw {} instance(s) {}->{} command recorded [Shader: {}, Material: {}, Mesh: {}]",
            self.accumulated_instance_count,
            self.accumulated_instance_range.start,
            self.accumulated_instance_range.end - 1,
            self.shader_name.name_style(),
            self.material_name.name_style(),
            self.mesh_name.name_style()
        );
    }

    /// Records the instances accumulated since the last draw as one indexed
    /// draw, if there are any, then empties the range.
    ///
    /// It runs wherever the accumulated instances stop sharing the bound
    /// state: before a shader, material, or mesh switch, before a range of
    /// instances that does not follow on, and at the end of the pass.
    pub fn record_draw_accumulated_instances(&mut self, render_pass: &mut wgpu::RenderPass) {
        if self.accumulated_instance_count > 0 {
            render_pass.draw_indexed(
                0..self.mesh_index_count,
                0,
                self.accumulated_instance_range.clone(),
            );
            self.log();
            self.accumulated_instance_range =
                self.accumulated_instance_range.end..self.accumulated_instance_range.end;
            self.accumulated_instance_count = 0;
        }
    }

    /// Adds the instance most recently prepared to the range the next draw
    /// covers.
    ///
    /// One call per queue item, so the range grows instance by instance until
    /// a state change or the end of the run records it.
    pub fn accumulate_instance(&mut self) {
        self.accumulated_instance_range =
            self.accumulated_instance_range.start..self.accumulated_instance_range.end + 1;
        self.accumulated_instance_count =
            self.accumulated_instance_range.end - self.accumulated_instance_range.start;
    }

    /// Draws what is accumulated, then starts accumulating at instance
    /// `position`: the first instance of a range that need not follow on from
    /// the one before it.
    pub fn start_run(&mut self, render_pass: &mut wgpu::RenderPass, position: u32) {
        self.record_draw_accumulated_instances(render_pass);
        self.accumulated_instance_range = position..position;
    }

    /// Binds the pipeline and bind groups the given shader draws through.
    ///
    /// The shader's own pipeline is used unless the pass supplies an override,
    /// and the engine and camera bind groups are set only when the shader
    /// declares that it reads them.
    pub fn change_shader(
        &mut self,
        renderer_resource_storage: &RendererResourceStorage,
        shader_handle: RendererShaderHandle,
        pipeline_override: Option<&wgpu::RenderPipeline>,
        render_pass: &mut wgpu::RenderPass,
        camera: &RendererCamera,
    ) {
        self.shader_handle = Some(shader_handle);
        let shader: &RendererShader = renderer_resource_storage
            .shaders
            .get(shader_handle)
            .unwrap();
        self.shader_name = shader.name.clone();

        debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "Changing shader to: {}", self.shader_name.name_style());

        // A pass that owns a pipeline draws through it: the pass decided the
        // target's format and its own depth and culling, and the pipeline its
        // shader was built with cannot know either.
        render_pass.set_pipeline(pipeline_override.unwrap_or(&shader.render_pipeline));

        if shader.pass_engine_parameters {
            render_pass.set_bind_group(
                ENGINE_PARAMETERS_BIND_GROUP_LAYOUT_INDEX,
                &renderer_resource_storage.engine_parameters.bind_group,
                &[],
            );
            debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "Engine parameters bound");
        }

        if shader.pass_camera_parameters {
            render_pass.set_bind_group(
                CAMERA_PARAMETERS_BIND_GROUP_LAYOUT_INDEX,
                &camera.bind_group,
                &[],
            );
            debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "Camera parameters bound");
        }

        debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "Renderer pipeline shader changed to: {}", self.shader_name.name_style());
    }

    /// Binds the parameter and texture bind groups the given material draws
    /// through.
    ///
    /// Either group may be absent, because a material without parameters or
    /// without textures declares none for it; a missing group leaves whatever
    /// was bound before it in place.
    pub fn change_material(
        &mut self,
        renderer_resource_storage: &RendererResourceStorage,
        material_handle: RendererMaterialHandle,
        render_pass: &mut wgpu::RenderPass,
    ) {
        self.material_handle = Some(material_handle);
        let material = renderer_resource_storage
            .materials
            .get(material_handle)
            .unwrap();
        self.material_name = material.name.clone();

        debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "Changing material to: {}", self.material_name.name_style());

        if let Some(ref parameters_bind_group) = material.parameters_bind_group {
            render_pass.set_bind_group(
                MATERIAL_PARAMETERS_BIND_GROUP_LAYOUT_INDEX,
                parameters_bind_group,
                &[],
            );
            debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "Material parameters bound");
        }

        if let Some(ref texture_bind_group) = material.textures_bind_group {
            render_pass.set_bind_group(
                MATERIAL_TEXTURES_BIND_GROUP_LAYOUT_INDEX,
                texture_bind_group,
                &[],
            );
            debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "Material textures bound");
        }

        debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "Renderer pipeline material changed to: {}", self.material_name.name_style());
    }

    /// Binds the vertex and index buffers of the given mesh.
    ///
    /// The mesh's index count is stored alongside the binding, because the
    /// draw calls read it from the context, not from the mesh.
    pub fn change_mesh(
        &mut self,
        renderer_resource_storage: &RendererResourceStorage,
        mesh_handle: RendererMeshHandle,
        render_pass: &mut wgpu::RenderPass,
    ) {
        self.mesh_handle = Some(mesh_handle);
        let mesh = renderer_resource_storage.meshes.get(mesh_handle).unwrap();
        self.mesh_name = mesh.name.clone();

        self.mesh_index_count = mesh.index_count;
        render_pass.set_vertex_buffer(0, mesh.vertex_buffer.slice(..));
        render_pass.set_index_buffer(mesh.index_buffer.slice(..), wgpu::IndexFormat::Uint32);

        debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "Renderer pipeline mesh changed to: {}", self.mesh_name.name_style());
    }
}

/// Owns the instance buffer and records a frame's mesh draws through it.
///
/// One drawer lives for the renderer's lifetime. The buffer is reused every
/// frame and grows only when a frame brings more instances than it holds.
pub(crate) struct MeshDrawer {
    /// How many instances the buffer grows by at a time.
    capacity_step: usize,
    instance_buffer: wgpu::Buffer,
    /// How many instances the buffer has room for.
    instance_capacity: usize,
}

impl MeshDrawer {
    /// Creates a drawer whose instance buffer has room for `capacity_step`
    /// instances, and grows in steps of that many.
    pub fn new(device: &wgpu::Device, capacity_step: usize) -> Self {
        MeshDrawer {
            capacity_step,
            instance_buffer: Self::allocate(device, capacity_step),
            instance_capacity: capacity_step,
        }
    }

    fn allocate(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instance_buffer"),
            size: (size_of::<Instance>() * capacity) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// Uploads the frame's instances, in queue order, in one write.
    ///
    /// Called once per frame before any pass records: every geometry pass
    /// draws from this one upload, addressing instances by queue position.
    /// Writes and the draws they feed land in one command buffer and every
    /// write runs first, so a buffer written per pass would leave all passes
    /// reading whichever pass wrote last.
    pub fn upload_instances(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        instances: &[Instance],
    ) {
        if instances.len() > self.instance_capacity {
            // Grown in whole steps, so a frame one instance over the limit
            // reallocates once rather than on every frame after it.
            let capacity = instances.len().div_ceil(self.capacity_step) * self.capacity_step;
            self.instance_buffer = Self::allocate(device, capacity);
            self.instance_capacity = capacity;
        }
        if !instances.is_empty() {
            queue.write_buffer(&self.instance_buffer, 0, bytemuck::cast_slice(instances));
        }
    }

    /// Opens a render pass on `encoder` and records the draws for the queue
    /// positions in `ranges`, then drops the pass before returning.
    ///
    /// The instances were uploaded by [`MeshDrawer::upload_instances`] in queue
    /// order, so a queue position is also the instance's index in the bound
    /// buffer and nothing is copied here. Consecutive items that share their
    /// shader, material and mesh become one instanced draw.
    ///
    /// # Errors
    ///
    /// Never returns an error: every step is a wgpu call that reports nothing
    /// to check, and `Ok(())` is the only value produced. The `Result` keeps
    /// the call site uniform with the frame path's other fallible steps.
    #[allow(clippy::too_many_arguments)]
    pub fn record_draw_commands(
        &mut self,
        // Resources
        encoder: &mut wgpu::CommandEncoder,
        renderer_resource_storage: &RendererResourceStorage,
        label: &str,
        pipeline: Option<&wgpu::RenderPipeline>,
        color_attachments: &[Option<wgpu::RenderPassColorAttachment>],
        depth_stencil_attachment: wgpu::RenderPassDepthStencilAttachment,
        // Rendering data
        camera: &RendererCamera,
        render_queue: &[RenderQueueItem],
        ranges: &[Range<u32>],
        viewport: RenderViewport,
        // Counts the pass's visible coverage and shader invocations, when GPU
        // profiling is on.
        profiler: Option<&Profiler>,
    ) -> Result<()> {
        let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some(label),
            color_attachments,
            depth_stencil_attachment: Some(depth_stencil_attachment),
            timestamp_writes: None,
            occlusion_query_set: profiler.and_then(|profiler| profiler.get_occlusion_query_set()),
        });

        render_pass.set_viewport(
            viewport.x as f32,
            viewport.y as f32,
            viewport.width as f32,
            viewport.height as f32,
            0.0,
            1.0,
        );
        render_pass.set_scissor_rect(viewport.x, viewport.y, viewport.width, viewport.height);
        render_pass.set_vertex_buffer(1, self.instance_buffer.slice(..));
        let counting = profiler
            .and_then(|profiler| profiler.begin_pipeline_statistics_query(&mut render_pass))
            .is_some();
        let occluding = profiler
            .and_then(|profiler| profiler.begin_occlusion_query(&mut render_pass))
            .is_some();

        let mut current_drawing_context = DrawingContext::default();
        // Key of the last item whose state was checked. It carries across
        // ranges, like the bound state it stands for.
        let mut previous_key: Option<u64> = None;

        for range in ranges {
            // A range starts a new run of instance indices: whatever the
            // previous range accumulated is drawn first.
            current_drawing_context.start_run(&mut render_pass, range.start);
            let items = &render_queue[range.start as usize..range.end as usize];

            for render_queue_item in items {
                // The key packs every handle the draw binds, so an item whose
                // key equals the one before it needs nothing rebound: it only
                // extends the current draw. The queue is sorted by key, so
                // that is nearly every item, and unpacking the key and
                // rebuilding three handles for each was most of this loop.
                if previous_key == Some(render_queue_item.key) {
                    current_drawing_context.accumulate_instance();
                    continue;
                }
                previous_key = Some(render_queue_item.key);

                let render_queue_key_fields = decompose_render_queue_key(render_queue_item.key);

                // Recreate resource handles
                let renderer_shader_handle = RendererShaderHandle::new(
                    render_queue_key_fields.shader_index.into(),
                    NonZeroU32::new(render_queue_key_fields.shader_version.into()).unwrap(),
                );
                let renderer_material_handle = RendererMaterialHandle::new(
                    render_queue_key_fields.material_index.into(),
                    NonZeroU32::new(render_queue_key_fields.material_version.into()).unwrap(),
                );
                let renderer_mesh_handle = RendererMeshHandle::new(
                    render_queue_key_fields.mesh_index.into(),
                    NonZeroU32::new(render_queue_key_fields.mesh_version.into()).unwrap(),
                );

                // Check for shader change
                if current_drawing_context.shader_handle != Some(renderer_shader_handle) {
                    current_drawing_context.record_draw_accumulated_instances(&mut render_pass);
                    current_drawing_context.change_shader(
                        renderer_resource_storage,
                        renderer_shader_handle,
                        pipeline,
                        &mut render_pass,
                        camera,
                    );
                }

                // Check for material change
                if current_drawing_context.material_handle != Some(renderer_material_handle) {
                    current_drawing_context.record_draw_accumulated_instances(&mut render_pass);
                    current_drawing_context.change_material(
                        renderer_resource_storage,
                        renderer_material_handle,
                        &mut render_pass,
                    );
                }

                // Check for mesh change
                if current_drawing_context.mesh_handle != Some(renderer_mesh_handle) {
                    current_drawing_context.record_draw_accumulated_instances(&mut render_pass);
                    current_drawing_context.change_mesh(
                        renderer_resource_storage,
                        renderer_mesh_handle,
                        &mut render_pass,
                    );
                }

                // Add new instance
                current_drawing_context.accumulate_instance();
            }
        }
        // Whatever the last range accumulated.
        current_drawing_context.record_draw_accumulated_instances(&mut render_pass);
        if let (true, Some(profiler)) = (counting, profiler) {
            profiler.end_pipeline_statistics_query(&mut render_pass);
        }
        if let (true, Some(profiler)) = (occluding, profiler) {
            profiler.end_occlusion_query(&mut render_pass);
        }

        // Drop render_pass before returning: the borrow of the encoder has to
        // end here, and the caller finishes the encoder.
        drop(render_pass);

        Ok(())
    }
}
