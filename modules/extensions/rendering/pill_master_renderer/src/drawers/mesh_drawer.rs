use std::{num::NonZeroU32, ops::Range};

use crate::error::Result;
use crate::{
    component::RenderViewport,
    config::{
        CAMERA_PARAMETERS_BIND_GROUP_LAYOUT_INDEX, ENGINE_PARAMETERS_BIND_GROUP_LAYOUT_INDEX,
        INITIAL_INSTANCE_VECTOR_CAPACITY, MATERIAL_PARAMETERS_BIND_GROUP_LAYOUT_INDEX,
        MATERIAL_TEXTURES_BIND_GROUP_LAYOUT_INDEX,
    },
    frame::RenderInstance,
    render_queue::{decompose_render_queue_key, RenderQueueItem},
    resources::{RendererCamera, RendererResourceStorage, RendererShader},
    slot_map::{RendererMaterialHandle, RendererMeshHandle, RendererShaderHandle},
    Instance,
};
use pill_core::{debug, PillStyle};

#[derive(Debug, Clone, Default)]
pub struct DrawingContext {
    rendering_order: u8,
    shader_handle: Option<RendererShaderHandle>,
    shader_name: String,
    material_handle: Option<RendererMaterialHandle>,
    material_name: String,
    mesh_handle: Option<RendererMeshHandle>,
    mesh_name: String,
    mesh_index_count: u32, // Number of indices in the current mesh

    accumulated_instance_range: Range<u32>,
    accumulated_instance_count: u32,

    rendering_context_change_number: u32,

    instance_batch_number: u32,
    instance_batch_size: u32,
}

impl DrawingContext {
    pub fn log(&self) {
        debug!(
            target: pill_core::telemetry::telemetry_target::RENDERING,
            "Draw {} instance(s) {}->{}/{} command recorded [Batch: {}, Rendering order: {}, Shader: {}, Material: {}, Mesh: {}]",
            self.accumulated_instance_count,
            self.accumulated_instance_range.start,
            self.accumulated_instance_range.end - 1,
            self.instance_batch_size,
            self.instance_batch_number,
            self.rendering_order,
            self.shader_name.name_style(),
            self.material_name.name_style(),
            self.mesh_name.name_style()
        );
    }

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

    pub fn accumulate_instance(&mut self) {
        self.accumulated_instance_range =
            self.accumulated_instance_range.start..self.accumulated_instance_range.end + 1;
        self.accumulated_instance_count =
            self.accumulated_instance_range.end - self.accumulated_instance_range.start;
    }

    pub fn change_rendering_order(&mut self, new_order: u8) {
        self.rendering_order = new_order;

        self.rendering_context_change_number += 1;
        debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "Rendering order changed to: {}", self.rendering_order);
    }

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

        self.rendering_context_change_number += 1;
        debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "Renderer pipeline shader changed to: {}", self.shader_name.name_style());
    }

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

        self.rendering_context_change_number += 1;
        debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "Renderer pipeline material changed to: {}", self.material_name.name_style());
    }

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

        self.rendering_context_change_number += 1;
        debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "Renderer pipeline mesh changed to: {}", self.mesh_name.name_style());
    }
}

pub struct MeshDrawer {
    max_instance_batch_size: u32,
    instances: Vec<Instance>,
    instance_buffer: wgpu::Buffer,
    /// How many instances the buffer has room for.
    instance_capacity: usize,
}

/// The region of the instance buffer one batch owns.
///
/// Every batch gets its own slice rather than sharing the front of the buffer:
/// the writes and the draws they feed are recorded into one command buffer, so a
/// shared region would leave every draw reading whichever batch was written
/// last. The offset is a whole number of batches, which keeps it four-byte
/// aligned the way `write_buffer` requires.
fn batch_region(
    batch_index: usize,
    batch_size: usize,
    instance_count: usize,
) -> std::ops::Range<u64> {
    let stride = size_of::<Instance>();
    let start = (batch_index * batch_size * stride) as u64;
    start..start + (instance_count * stride) as u64
}

impl MeshDrawer {
    pub fn new(device: &wgpu::Device, max_instance_batch_size: u32) -> Self {
        let capacity = max_instance_batch_size as usize;
        let instance_buffer = Self::allocate(device, capacity);

        MeshDrawer {
            max_instance_batch_size,
            instances: Vec::<Instance>::with_capacity(INITIAL_INSTANCE_VECTOR_CAPACITY),
            instance_buffer,
            instance_capacity: capacity,
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

    /// Make room for a frame's instances, one region per batch.
    ///
    /// The capacity grows in whole batches, so a frame that is one instance over
    /// the limit reallocates once rather than on every frame after it.
    fn ensure_capacity(&mut self, device: &wgpu::Device, instances: usize) {
        if instances <= self.instance_capacity {
            return;
        }

        let batch_size = self.max_instance_batch_size as usize;
        let capacity = instances.div_ceil(batch_size) * batch_size;
        self.instance_buffer = Self::allocate(device, capacity);
        self.instance_capacity = capacity;
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_draw_commands(
        &mut self,
        // Resources
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        renderer_resource_storage: &RendererResourceStorage,
        label: &str,
        pipeline: Option<&wgpu::RenderPipeline>,
        color_attachments: &[Option<wgpu::RenderPassColorAttachment>],
        depth_stencil_attachment: wgpu::RenderPassDepthStencilAttachment,
        // Rendring data
        camera: &RendererCamera,
        render_queue: &[RenderQueueItem],
        instances: &[RenderInstance],
        viewport: RenderViewport,
        // profiler: &mut Profiler,
    ) -> Result<()> {
        //let _timestamp_query_start = profiler.write_timestamp(encoder, "xx");

        let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some(label),
            color_attachments,
            depth_stencil_attachment: Some(depth_stencil_attachment),
            timestamp_writes: None,
            occlusion_query_set: None, // profiler.get_occlusion_query_set(), // immut borrow ends after this stmt
                                       //timestamp_writes: None,
                                       // occlusion_query_set: profiler.get_occlusion_query_set(), // immut borrow ends after this stmt
                                       // timestamp_writes: Some(wgpu::RenderPassTimestampWrites {
                                       //     query_set: profiler.get_timestamp_query_set().unwrap(),
                                       //     beginning_of_pass_write_index: Some(0),
                                       //     end_of_pass_write_index: Some(1),
                                       // }),
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

        // let _pipeline_statistics_query_start = profiler.begin_pipeline_statistics_query(&mut render_pass);
        //let _occlusion_query_start = profiler.begin_occlusion_query(&mut render_pass);

        // let mut current_rendering_order: u8 = 0;

        // let mut current_shader_handle: Option<RendererShaderHandle> = None;
        // let mut current_shader_name = "";
        // let mut current_material_handle: Option<RendererMaterialHandle> = None;
        // let mut current_material_name = "";
        // let mut current_mesh_handle: Option<RendererMeshHandle> = None;
        // let mut current_mesh_name = "";
        // let mut current_mesh_index_count: u32 = 0; // Number of indices in the current mesh

        let mut current_drawing_context = DrawingContext::default();

        self.ensure_capacity(device, render_queue.len());
        let batch_size = self.max_instance_batch_size as usize;

        for (i, instance_batch) in render_queue.chunks(batch_size).enumerate() {
            let batch_size = instance_batch.len();
            current_drawing_context.instance_batch_number = i as u32;
            current_drawing_context.instance_batch_size = batch_size as u32;

            // Prepare instance data and load it to buffer
            self.instances.clear();
            self.instances.reserve(instance_batch.len()); // Pre-allocate exact capacity

            for render_queue_item in instance_batch {
                let transform_component =
                    &instances[render_queue_item.entity_index as usize].transform;
                //println!("Creating new instance with transform component: {:?} {:?}", i, transform_component.position);
                self.instances.push(Instance::new(transform_component));
            }

            let region = batch_region(i, batch_size, self.instances.len());
            queue.write_buffer(
                &self.instance_buffer,
                region.start,
                bytemuck::cast_slice(&self.instances),
            ); // Update this batch's region of the instance buffer

            render_pass.set_vertex_buffer(1, self.instance_buffer.slice(region)); // Set instance buffer

            // Reset instance range for each batch
            current_drawing_context.accumulated_instance_range = 0..0;
            current_drawing_context.accumulated_instance_count = 0;

            for (j, render_queue_item) in instance_batch.iter().enumerate() {
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

                // Check for rendering order change
                if current_drawing_context.rendering_order > render_queue_key_fields.order {
                    current_drawing_context.record_draw_accumulated_instances(&mut render_pass);
                    current_drawing_context.change_rendering_order(render_queue_key_fields.order);
                }

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

                // If last in batch, draw accumulated instances
                if j == batch_size - 1 {
                    current_drawing_context.record_draw_accumulated_instances(&mut render_pass);
                }
            }
        }

        // Drop render_pass before finishing encoder
        //let _occlusion_query_end = profiler.end_occlusion_query(&mut render_pass);
        //let _pipeline_statistics_query_end = profiler.end_pipeline_statistics_query(&mut render_pass);

        drop(render_pass);

        // let _timestamp_query_end = profiler.write_timestamp(encoder, "xx12");

        //queue.submit(std::iter::once(encoder.finish()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_batch_owns_a_region_of_its_own() {
        let stride = size_of::<Instance>() as u64;

        let first = batch_region(0, 4, 4);
        let second = batch_region(1, 4, 4);
        let last = batch_region(2, 4, 2);

        assert_eq!(first, 0..4 * stride);
        assert_eq!(second, 4 * stride..8 * stride);
        assert_eq!(last, 8 * stride..10 * stride);
        assert!(
            first.end <= second.start && second.end <= last.start,
            "two batches sharing a region is the bug this exists to prevent"
        );
    }

    #[test]
    fn a_region_starts_where_the_buffer_can_be_written() {
        // `write_buffer` refuses an offset that is not a multiple of four, and
        // an instance is not a power of two in size.
        for batch in 0..4 {
            assert_eq!(batch_region(batch, 3, 3).start % 4, 0);
        }
    }
}
