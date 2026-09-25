use crate::error::Result;
use wgpu::util::DeviceExt;

// Layout must match the HLSL `EngineParams` in `include/common.hlsl` (std140):
//   vec3  fog_color;       // offset 0  (12 bytes)
//   float fog_density;     // offset 12 (4 bytes)
//   vec4  frame_size;      // offset 16; xy = pixels, zw = its reciprocal
//   vec4  time;            // offset 32; x = seconds, y = delta, z = frame
//   // total: 48 bytes
#[repr(C)]
#[derive(Debug, Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct EngineParametersData {
    pub fog_color: [f32; 3],
    pub fog_density: f32,
    /// The surface size in pixels, and its reciprocal.
    ///
    /// A pass that samples the frame at a texel offset needs this. Reading it
    /// from here rather than from the pass's own parameters is what keeps a
    /// resized window from sampling at the size it was when the chain was
    /// built, and it is why a pass does not have to be handed the frame size by
    /// the game at all.
    pub frame_size: [f32; 4],
    /// x = seconds since the first frame, y = this frame's duration, z = the
    /// frame's sequence number. w is unused.
    pub time: [f32; 4],
}

impl Default for EngineParametersData {
    fn default() -> Self {
        Self::new()
    }
}

impl EngineParametersData {
    pub fn new() -> Self {
        Self {
            fog_color: [0.0; 3],
            fog_density: 0.0,
            frame_size: [0.0; 4],
            time: [0.0; 4],
        }
    }

    /// The same values the shaders read, in the order the struct declares them.
    pub fn update_data(
        &mut self,
        fog_density: f32,
        fog_color: [f32; 3],
        frame_size: [u32; 2],
        time: [f32; 4],
    ) {
        self.fog_density = fog_density;
        self.fog_color = fog_color;
        self.frame_size = [
            frame_size[0] as f32,
            frame_size[1] as f32,
            1.0 / frame_size[0].max(1) as f32,
            1.0 / frame_size[1].max(1) as f32,
        ];
        self.time = time;
    }
}

// --- Camera ---

#[derive(Debug)]
pub struct EngineParameters {
    pub parameters_data: EngineParametersData,
    pub parameters_uniform_buffer: wgpu::Buffer,
    pub bind_group_layout: wgpu::BindGroupLayout,
    pub bind_group: wgpu::BindGroup,
}

impl EngineParameters {
    pub fn new(device: &wgpu::Device) -> Result<Self> {
        let parameters_data = EngineParametersData::new();

        let parameters_uniform_buffer =
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("engine_parameters_buffer"),
                contents: bytemuck::cast_slice(&[parameters_data]),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            });

        // Define engine bind group layout
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("engine_parameters_bind_group_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0, // (set = X, binding = 0)
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false, // Specifies if this buffer will be changing size or not
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0, // (set = X, binding = 0)
                resource: parameters_uniform_buffer.as_entire_binding(),
            }],
            label: Some("engine_parameters_bind_group"),
        });

        let camera = Self {
            parameters_data,
            parameters_uniform_buffer,
            bind_group_layout,
            bind_group,
        };

        Ok(camera)
    }

    pub fn update(
        &mut self,
        queue: &wgpu::Queue,
        fog_density: f32,
        fog_color: [f32; 3],
        frame_size: [u32; 2],
        time: [f32; 4],
    ) {
        self.parameters_data
            .update_data(fog_density, fog_color, frame_size, time);
        queue.write_buffer(
            &self.parameters_uniform_buffer,
            0,
            bytemuck::cast_slice(&[self.parameters_data]),
        );
    }
}
