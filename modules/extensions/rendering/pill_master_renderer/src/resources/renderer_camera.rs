use crate::{
    component::{CameraComponent, TransformComponent},
    error::Result,
};
use pill_core::math::{Matrix4f, Vector3f, Vector4f};
use wgpu::util::DeviceExt;

// --- Camera Uniform ---

#[repr(C)]
#[derive(Debug, Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct CameraParametersData {
    pub position: Vector4f,               // Camera position
    pub view_projection_matrix: Matrix4f, // Perspective manipulation
}

impl Default for CameraParametersData {
    fn default() -> Self {
        Self::new()
    }
}

impl CameraParametersData {
    pub fn new() -> Self {
        Self {
            position: Vector4f::ZERO,
            view_projection_matrix: Matrix4f::IDENTITY,
        }
    }

    pub fn update_data(
        &mut self,
        camera_component: &CameraComponent,
        transform_component: &TransformComponent,
        aspect: f32,
    ) {
        // Update position
        self.position = Vector4f::new(
            transform_component.translation[0],
            transform_component.translation[1],
            transform_component.translation[2],
            0.0,
        );

        // Update view-projection
        self.view_projection_matrix =
            CameraParametersData::calculate_projection_matrix(camera_component, aspect)
                * CameraParametersData::calculate_view_matrix(transform_component);
    }

    fn calculate_view_matrix(transform_component: &TransformComponent) -> Matrix4f {
        let position = Vector3f::from_array(transform_component.translation);
        let rotation = glam::Quat::from_array(transform_component.rotation);
        let rotation = if rotation.is_finite() && rotation.length_squared() > 1e-8 {
            rotation.normalize()
        } else {
            glam::Quat::IDENTITY
        };
        glam::camera::rh::view::look_to_mat4(
            position,
            rotation * Vector3f::NEG_Z,
            rotation * Vector3f::Y,
        )
    }

    fn calculate_projection_matrix(camera_component: &CameraComponent, aspect: f32) -> Matrix4f {
        glam::camera::rh::proj::directx::perspective(
            camera_component.vertical_fov.to_radians(),
            aspect,
            camera_component.near,
            camera_component.far,
        )
    }
}

// --- Camera ---

#[derive(Debug)]
pub struct RendererCamera {
    pub parameters_data: CameraParametersData,
    pub parameters_uniform_buffer: wgpu::Buffer,
    pub bind_group_layout: wgpu::BindGroupLayout,
    pub bind_group: wgpu::BindGroup,
}

impl RendererCamera {
    pub fn new(
        device: &wgpu::Device,
        camera_bind_group_layout: wgpu::BindGroupLayout,
    ) -> Result<Self> {
        let parameters_data = CameraParametersData::new();

        let parameters_uniform_buffer =
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("camera_parameters_buffer"),
                contents: bytemuck::cast_slice(&[parameters_data]),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &camera_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0, // (set = X, binding = 0)
                resource: parameters_uniform_buffer.as_entire_binding(),
            }],
            label: Some("camera_parameters_bind_group"),
        });

        let camera = Self {
            parameters_data,
            parameters_uniform_buffer,
            bind_group_layout: camera_bind_group_layout,
            bind_group,
        };

        Ok(camera)
    }

    pub fn update(
        &mut self,
        queue: &wgpu::Queue,
        camera_component: &CameraComponent,
        transform_component: &TransformComponent,
        aspect: f32,
    ) {
        self.parameters_data
            .update_data(camera_component, transform_component, aspect);
        queue.write_buffer(
            &self.parameters_uniform_buffer,
            0,
            bytemuck::cast_slice(&[self.parameters_data]),
        );
    }
}
