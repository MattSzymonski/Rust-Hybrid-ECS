//! The engine-wide uniform block a pass may read.
//!
//! # Responsibilities
//!
//! - Define the layout the GPU sees ([`EngineParametersData`]) and keep the
//!   CPU-side copy the renderer writes each frame in step with it.
//! - Own the uniform buffer that carries those values, together with its bind
//!   group layout and bind group ([`EngineParameters`]).
//!
//! # Design
//!
//! One instance lives on the renderer and is refilled per frame:
//! [`EngineParameters::update`] rewrites the whole buffer, so the CPU-side
//! copy and the GPU copy stay in step without anyone tracking dirty fields.
//! The fog pair leads the block because the HLSL `EngineParams` in
//! `include/common.hlsl` declares it at those offsets; the frame size and time
//! follow as the engine side's extension, which no shipped shader reads yet.

// External crates
use wgpu::util::DeviceExt;

// Current crate
use crate::error::Result;

/// The engine-wide uniform block a pass may read, in its GPU layout.
///
/// Fog and the frame's size and time are frame state rather than pass state,
/// so every pass reads the same buffer instead of carrying copies that could
/// disagree within one frame.
// The fog pair must match the HLSL `EngineParams` in `include/common.hlsl`
// (std140, and that is all it declares so far):
//   vec3  fog_color;       // offset 0  (12 bytes)
//   float fog_density;     // offset 12 (4 bytes)
// The frame size and time are the engine side's extension, laid out after the
// pair; no shipped HLSL or WGSL declares them yet.
//   vec4  frame_size;      // offset 16; xy = pixels, zw = its reciprocal
//   vec4  time;            // offset 32; x = seconds, y = delta, z = frame
//   // struct total: 48 bytes
#[repr(C)]
#[derive(Debug, Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct EngineParametersData {
    /// Colour the lit shaders blend toward with distance.
    pub fog_color: [f32; 3],
    /// Fog thickness: how quickly the colour builds up with distance.
    pub fog_density: f32,
    /// The surface size in pixels, and its reciprocal.
    ///
    /// For a pass that samples the frame at a texel offset. No shipped shader
    /// reads it yet, so it waits for the pass that will: taking the size from
    /// the buffer keeps it current across a resize without the game handing it
    /// through pass parameters.
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
    /// Creates an all-zero set of parameters.
    ///
    /// The uniform buffer is created from these values, so this is what every
    /// shader reads until the first frame's `update_data` call replaces them.
    pub fn new() -> Self {
        Self {
            fog_color: [0.0; 3],
            fog_density: 0.0,
            frame_size: [0.0; 4],
            time: [0.0; 4],
        }
    }

    /// Writes one frame's values into the buffer data, converting the surface
    /// size on the way in.
    ///
    /// The size arrives as pixel dimensions and leaves as pixels plus the
    /// reciprocal, which is the form the layout declares and the form a shader
    /// sampling at a texel offset wants. Both reciprocals are taken over a
    /// dimension clamped to at least one pixel, so a minimized or
    /// not-yet-sized surface cannot leave an infinity in the buffer for a
    /// shader to read back per pixel.
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

// --- Engine Parameters ---

/// The engine parameters as the renderer owns them: the values, the buffer
/// they are uploaded to, and the bindings that expose them.
///
/// Built once when the renderer starts and refilled per frame through
/// [`update`](EngineParameters::update). Every pass that reads engine
/// parameters binds this one group, so all of them necessarily see the same
/// fog and frame state.
#[derive(Debug)]
pub struct EngineParameters {
    /// The values currently held in the buffer, kept CPU-side so `update` can
    /// rewrite the buffer.
    pub parameters_data: EngineParametersData,
    /// Uniform buffer at binding 0 holding one [`EngineParametersData`].
    pub parameters_uniform_buffer: wgpu::Buffer,
    /// Layout a pipeline declares for the engine group, so the one bind group
    /// below works for every pass that reads these values.
    pub bind_group_layout: wgpu::BindGroupLayout,
    /// The group binding the buffer, built from the layout above.
    pub bind_group: wgpu::BindGroup,
}

impl EngineParameters {
    /// Creates the uniform buffer and its bindings that carry the engine
    /// parameters to every shader.
    ///
    /// The values start zeroed, and the layout declares a single uniform
    /// buffer at binding 0 that both the vertex and fragment stages can read.
    /// A pipeline that reads engine parameters must be built against
    /// [`bind_group_layout`](EngineParameters::bind_group_layout), since that
    /// is what makes this one group bindable by every pass.
    ///
    /// # Errors
    ///
    /// None of its own: creating a buffer, a layout, and a bind group only
    /// records work, and wgpu reports a rejected descriptor through the
    /// device's error handler rather than through this return value.
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

    /// Refills the buffer with a frame's values.
    ///
    /// The surface size arrives as pixels; `update_data` stores it in the form
    /// the shaders read, and the write is queued before any command buffer
    /// submitted afterwards, so every pass recorded later in the frame sees
    /// the new values.
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
