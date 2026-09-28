//! What one material becomes on the GPU: the bind groups its draws bind and
//! the packers that fill them.
//!
//! # Responsibilities
//!
//! - Build what a material's draws need: the parameters bind group over the
//!   values its shader reads and the textures bind group over the slots it
//!   declares ([`RendererMaterial`]).
//! - Pack uniform values one 16-byte slot each, in the order the shader
//!   declares them. A pass packs through the same two functions, so the two
//!   cannot drift apart on what they hand a shader.
//! - Fall back to the renderer's default color and normal textures for slots
//!   a material leaves unbound, and refuse a depth slot instead of filling
//!   it: depth belongs to a pass, the only thing that can hand it to a
//!   shader.
//!
//! # Design
//!
//! Built during asset sync, when the material's own content version moves,
//! not per frame - a frame only binds what was built. A shader or texture
//! rebuilt under a material takes the material with it: the sync forgets the
//! materials that read the rebuilt key, so this path runs again against the
//! new object. The struct keeps handles and bind groups, never the uniform
//! buffer itself: a bind group holds what it references, so the buffer lives
//! exactly as long as the group that reads it.

// Standard library
use std::collections::HashMap;

// External crates
use indexmap::IndexMap;
use pill_core::{debug, PillStyle};

// Current crate
use crate::{
    assets::{
        MaterialParameter, ShaderParameterSlot, ShaderParameterType, ShaderTextureSlot, TextureType,
    },
    error::{capturing_validation, RendererError, Result},
    slot_map::{RendererShaderHandle, RendererTextureHandle},
};

use crate::resources::RendererResourceStorage;

// --- Material ---

/// One material's GPU resources: the bind groups its draws bind.
///
/// The game-facing [`Material`](crate::Material) asset says what a surface
/// looks like; this is what that description becomes on the GPU once the
/// shader is known. One is built per material during asset sync and stored
/// beside every other GPU resource.
pub struct RendererMaterial {
    /// Label used in logs and error messages, taken from the material asset.
    pub name: String,
    /// Shader the bind groups were built against.
    ///
    /// Kept because the frame's queue packs it into the sort key: draws that
    /// share a shader end up adjacent, and the drawer rebinds pipeline state
    /// once per run instead of once per draw.
    pub shader_handle: RendererShaderHandle,
    /// Values the shader reads, bound at group 2.
    ///
    /// Built from the shader's declared parameter slots, so a shader that
    /// declares none leaves this `None` rather than an empty group.
    pub parameters_bind_group: Option<wgpu::BindGroup>,
    /// Textures the shader samples, bound at group 3.
    ///
    /// One view and one sampler per declared slot. A slot the material left
    /// unbound holds the renderer's default texture for that slot's type.
    pub textures_bind_group: Option<wgpu::BindGroup>,
}

impl RendererMaterial {
    /// Builds one material's bind groups against the shader it names.
    ///
    /// Both groups follow the shader's declared slots rather than the
    /// material's maps, so a value or texture bound under a name the shader
    /// never declares is ignored, and a declared slot the material leaves
    /// unbound falls back to its default. A shader that declares no slots of
    /// a kind gets no group of that kind.
    ///
    /// # Errors
    ///
    /// Returns [`RendererError::RendererResourceNotFound`] when the shader
    /// handle names no loaded shader, and [`RendererError::Other`] when a
    /// bind group fails to build - validation is scoped so a mismatch between
    /// what the shader declares and what the material supplies is reported
    /// against the material instead of panicking through wgpu's
    /// uncaptured-error handler. A texture slot declared as depth is also an
    /// error: only a pass can read the renderer's depth buffer.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        rendering_resource_storage: &RendererResourceStorage,
        name: &str,
        shader_handle: RendererShaderHandle,
        textures: &[(String, RendererTextureHandle)],
        parameters: &HashMap<String, MaterialParameter>,
    ) -> Result<Self> {
        debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "Creating material {}", name.name_style());

        let shader = rendering_resource_storage
            .shaders
            .get(shader_handle)
            .ok_or(RendererError::RendererResourceNotFound)?;

        let parameter_slots = &shader.parameter_slots;
        let texture_slots = &shader.texture_slots;

        // Create parameters uniform buffer and bind group if there are parameter slots
        let parameters_bind_group = {
            if !parameter_slots.is_empty() {
                // Calculate uniform buffer size, create buffer if needed and write data to it
                let parameters_uniform_buffer_size = Self::calculate_uniform_size(parameter_slots);

                let parameters_uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(&format!("{}_material_parameters_buffer", name)),
                    size: parameters_uniform_buffer_size as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });

                // Write parameter data to buffer
                Self::write_parameters_to_buffer(
                    queue,
                    &parameters_uniform_buffer,
                    parameter_slots,
                    parameters,
                )?;

                debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "Uniform buffer of size {} bytes created", parameters_uniform_buffer_size);

                // Create parameters uniform buffer bind group; scoped so a
                // binding mismatch is reported against the material instead of
                // panicking through the uncaptured-error handler.
                let parameters_bind_group = capturing_validation(device, || {
                    device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some(&format!("{}_material_parameters_bind_group", name)),
                        layout: shader.parameters_bind_group_layout.as_ref().unwrap(),
                        entries: &[wgpu::BindGroupEntry {
                            binding: 0, // (set = 2, binding = 0)
                            resource: parameters_uniform_buffer.as_entire_binding(),
                        }],
                    })
                })
                .map_err(|detail| RendererError::Other {
                    detail: format!("material `{name}` parameters bind group: {detail}"),
                })?;

                debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "Parameters bind group created");

                // The buffer is not stored: the bind group holds what it
                // references, so it lives as long as the group that reads it.
                Some(parameters_bind_group)
            } else {
                debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "No parameter slots found, skipping uniform buffer and bind group creation");
                None
            }
        };

        // Create texture bind group
        let textures_bind_group = if !texture_slots.is_empty() {
            Some(Self::create_textures_bind_group(
                device,
                rendering_resource_storage,
                shader.textures_bind_group_layout.as_ref().unwrap(),
                &format!("{}_textures", name),
                texture_slots,
                textures,
            )?)
        } else {
            None
        };

        debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "Textures bind group created");

        let renderer_material = Self {
            name: name.to_string(),
            shader_handle,
            parameters_bind_group,
            textures_bind_group,
        };

        debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "Material creation successful");

        Ok(renderer_material)
    }

    /// Returns the uniform buffer size a shader's parameter slots need.
    ///
    /// Every slot is one `vec4`-aligned 16-byte region in WGSL, so the size
    /// is the slot count times 16. Passes size their parameter buffers
    /// through this too, which is what stops a material and a pass drifting
    /// apart on what they promise a shader.
    pub(crate) fn calculate_uniform_size(
        parameter_slots: &IndexMap<String, ShaderParameterSlot>,
    ) -> usize {
        parameter_slots.len() * 16
    }

    /// Packs a material's values into the uniform buffer its shader reads.
    ///
    /// Walks the shader's slots in declaration order and writes one 16-byte
    /// region per slot, so a value is placed by the name the shader gave it
    /// rather than by position in the map. A declared slot with no value
    /// writes as zero, which keeps a partially filled material drawing
    /// instead of refusing it.
    ///
    /// # Errors
    ///
    /// Infallible today: a slot with no value packs as zero rather than
    /// being refused, so no input reaches an error path.
    pub(crate) fn write_parameters_to_buffer(
        queue: &wgpu::Queue,
        buffer: &wgpu::Buffer,
        parameter_slots: &IndexMap<String, ShaderParameterSlot>,
        parameters: &HashMap<String, MaterialParameter>,
    ) -> Result<()> {
        // Stage every slot in one vector so the GPU buffer is written once.
        let mut data = Vec::new();

        // NOTE: Each parameter takes a full 16-byte slot (vec4 alignment in
        // WGSL), padding included. Packing by type would use less buffer
        // space, but the fixed slot is simpler and keeps a material and a
        // pass packed identically.
        for (slot_name, slot) in parameter_slots {
            match slot.parameter_type {
                ShaderParameterType::Color => {
                    // Color parameter (3 floats + padding)
                    if let Some(MaterialParameter::Color(value)) = parameters.get(slot_name) {
                        data.extend_from_slice(&value[0].to_le_bytes());
                        data.extend_from_slice(&value[1].to_le_bytes());
                        data.extend_from_slice(&value[2].to_le_bytes());
                        data.extend_from_slice(&0.0f32.to_le_bytes()); // Padding
                    } else {
                        data.extend_from_slice(&[0u8; 16]);
                    }
                }
                ShaderParameterType::Scalar => {
                    // Scalar parameter (1 float + padding)
                    if let Some(MaterialParameter::Scalar(value)) = parameters.get(slot_name) {
                        data.extend_from_slice(&value.to_le_bytes());
                        data.extend_from_slice(&[0u8; 12]); // Padding to 16 bytes
                    } else {
                        data.extend_from_slice(&[0u8; 16]);
                    }
                }
                ShaderParameterType::Bool => {
                    // Bool parameter (1 u32 + padding)
                    if let Some(MaterialParameter::Bool(value)) = parameters.get(slot_name) {
                        let value: u32 = if *value { 1 } else { 0 };
                        data.extend_from_slice(&value.to_le_bytes());
                        data.extend_from_slice(&[0u8; 12]); // Padding to 16 bytes
                    } else {
                        data.extend_from_slice(&[0u8; 16]);
                    }
                }
            }
        }

        if !data.is_empty() {
            queue.write_buffer(buffer, 0, &data);
        }

        Ok(())
    }

    /// Builds the textures bind group for a material's declared slots.
    ///
    /// Walks the shader's slots rather than the material's bindings, so each
    /// entry lands at the binding the shader declared and a slot the material
    /// left unbound falls back to the renderer's default color or normal
    /// texture. Depth is refused rather than filled: a pass is the only thing
    /// that can hand a shader a depth buffer, so a material shader that asks
    /// for one has a mistake worth naming.
    ///
    /// # Errors
    ///
    /// Returns [`RendererError::Other`] for a depth slot, or when wgpu
    /// rejects the group - creation is scoped so the failure names the
    /// material instead of panicking through the uncaptured-error handler.
    fn create_textures_bind_group(
        device: &wgpu::Device,
        rendering_resource_storage: &RendererResourceStorage,
        texture_bind_group_layout: &wgpu::BindGroupLayout,
        name: &str,
        texture_slots: &IndexMap<String, ShaderTextureSlot>,
        textures: &[(String, RendererTextureHandle)],
    ) -> Result<wgpu::BindGroup> {
        let mut entries = Vec::new();

        for (slot_name, slot) in texture_slots {
            // Get texture from material texture map or use default
            let renderer_texture_handle = match textures
                .iter()
                .find(|(k, _)| k == slot_name)
                .map(|(_, handle)| *handle)
            {
                Some(handle) => {
                    debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "Material texture slot {} found in material textures", slot_name.name_style());
                    handle
                }
                None => {
                    debug!(target: pill_core::telemetry::telemetry_target::RENDERING, "Material texture slot {} not found in material textures, using default texture", slot_name.name_style());
                    match slot.texture_type {
                        TextureType::Color => rendering_resource_storage.default_color_texture,
                        TextureType::Normal => rendering_resource_storage.default_normal_texture,
                        // A material has no depth to give: only the renderer's own
                        // buffer holds any, and a pass is what reads it. A
                        // material shader that asks for depth is a mistake worth
                        // naming rather than a slot quietly filled with white.
                        TextureType::Depth => {
                            return Err(RendererError::Other {
                                detail: format!(
                                    "material `{name}` declares texture slot `{slot_name}` as depth, which only a pass can read"
                                ),
                            });
                        }
                    }
                }
            };

            let texture = rendering_resource_storage
                .textures
                .get(renderer_texture_handle)
                .unwrap();

            // Add texture view entry
            entries.push(wgpu::BindGroupEntry {
                binding: slot.texture_binding,
                resource: wgpu::BindingResource::TextureView(&texture.texture_view),
            });

            // Add sampler entry
            entries.push(wgpu::BindGroupEntry {
                binding: slot.sampler_binding,
                resource: wgpu::BindingResource::Sampler(&texture.sampler),
            });
        }

        // Set texture resources to the bind group; scoped like every other
        // creation so a mismatch between the shader's declared slots and the
        // material's textures names the material instead of panicking.
        capturing_validation(device, || {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                layout: texture_bind_group_layout,
                entries: &entries,
                label: Some(name),
            })
        })
        .map_err(|detail| RendererError::Other {
            detail: format!("material `{name}` textures bind group: {detail}"),
        })
    }
}
