//! What one material becomes on the GPU: the bind groups its draws bind and
//! the packers that fill them.
//!
//! # Responsibilities
//!
//! - Build the parameters and textures bind groups a material's draws bind,
//!   following the slots its shader declares ([`RendererMaterial`]).
//! - Pack uniform values one 16-byte slot each, in declaration order. A pass
//!   packs through the same two functions, so a material and a pass cannot
//!   hand a shader different layouts.
//! - Fall back to the renderer's default texture for a slot's type, and
//!   refuse a depth slot: depth belongs to a pass, the only thing that can
//!   hand it to a shader. A texture whose shape (2D or cube) differs from its
//!   slot's is refused by name.
//!
//! # Design
//!
//! Built during asset sync, when the material's own content version moves,
//! not per frame - a frame only binds what was built. A shader or texture
//! rebuilt under a material takes the material with it, because the sync
//! forgets the materials that read the rebuilt key. The struct keeps bind
//! groups, never the uniform buffer: a bind group holds what it references,
//! so the buffer lives exactly as long as the group that reads it.

// Standard library
use std::collections::BTreeMap;

// External crates
use indexmap::IndexMap;
use pill_core::debug;

// Current crate
use crate::{
    assets::{MaterialParameter, ShaderParameterSlot, ShaderParameterType, ShaderTextureSlot},
    error::{capturing_validation, RendererError, Result},
    resources::{
        check_slot_shape, RendererResourceStorage, RendererShaderHandle, RendererTextureHandle,
    },
};

// --- Handle ---

pill_core::define_slot_key!(RendererMaterialHandle);

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
    /// Both groups follow the shader's declared slots rather than the material's
    /// maps: a value or texture bound under a name the shader never declares is
    /// ignored, and a declared slot left unbound falls back to its default.
    ///
    /// # Errors
    ///
    /// Returns [`RendererError::RendererResourceNotFound`] when the handle
    /// names no loaded shader, [`RendererError::Other`] when a bind group
    /// fails to build, including a depth texture slot: only a pass can read
    /// the depth buffer.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        rendering_resource_storage: &RendererResourceStorage,
        name: &str,
        shader_handle: RendererShaderHandle,
        textures: &[(String, RendererTextureHandle)],
        parameters: &BTreeMap<String, MaterialParameter>,
    ) -> Result<Self> {
        let shader = rendering_resource_storage
            .shaders
            .get(shader_handle)
            .ok_or(RendererError::RendererResourceNotFound)?;

        let parameter_slots = &shader.parameter_slots;
        let texture_slots = &shader.texture_slots;

        let parameters_bind_group = {
            if !parameter_slots.is_empty() {
                let parameters_uniform_buffer_size = Self::calculate_uniform_size(parameter_slots);

                let parameters_uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(&format!("{}_material_parameters_buffer", name)),
                    size: parameters_uniform_buffer_size as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });

                Self::write_parameters_to_buffer(
                    queue,
                    &parameters_uniform_buffer,
                    parameter_slots,
                    parameters,
                )?;

                // Scoped so a binding mismatch is reported against the material
                // instead of panicking through the uncaptured-error handler.
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

                // The buffer is not stored: the bind group holds what it
                // references, so it lives as long as the group that reads it.
                Some(parameters_bind_group)
            } else {
                None
            }
        };

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

        let renderer_material = Self {
            name: name.to_string(),
            shader_handle,
            parameters_bind_group,
            textures_bind_group,
        };

        // One line per material, hidden unless `engine::rendering` is at debug.
        let defaulted_textures = texture_slots
            .keys()
            .filter(|slot_name| !textures.iter().any(|(name, _)| name == *slot_name))
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        debug!(
            target: pill_core::telemetry::telemetry_target::RENDERING,
            material = name,
            shader = shader.name.as_str(),
            defaulted_textures = %defaulted_textures,
            "material created"
        );

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
    /// rather than by position in the map.
    ///
    /// # Errors
    ///
    /// Never returns an error: a declared slot with no value packs as zero,
    /// which keeps a partially filled material drawing, and the pass path
    /// calls this same function.
    pub(crate) fn write_parameters_to_buffer(
        queue: &wgpu::Queue,
        buffer: &wgpu::Buffer,
        parameter_slots: &IndexMap<String, ShaderParameterSlot>,
        parameters: &BTreeMap<String, MaterialParameter>,
    ) -> Result<()> {
        // Stage every slot in one vector so the GPU buffer is written once.
        let mut data = Vec::new();

        // Each parameter takes a full 16-byte slot (vec4 alignment in WGSL),
        // padding included. Packing by type would use less buffer space, but
        // the fixed slot is simpler and keeps a material and a pass packed
        // identically.
        for (slot_name, slot) in parameter_slots {
            match slot.parameter_type {
                ShaderParameterType::Color => {
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
                    if let Some(MaterialParameter::Scalar(value)) = parameters.get(slot_name) {
                        data.extend_from_slice(&value.to_le_bytes());
                        data.extend_from_slice(&[0u8; 12]); // Padding to 16 bytes
                    } else {
                        data.extend_from_slice(&[0u8; 16]);
                    }
                }
                ShaderParameterType::Bool => {
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
    /// texture. Depth is refused rather than filled: only a pass can hand a
    /// shader a depth buffer.
    ///
    /// # Errors
    ///
    /// Returns [`RendererError::Other`] for a depth slot, or when wgpu rejects
    /// the group - creation is scoped so the failure names the material.
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
            let renderer_texture_handle = match textures
                .iter()
                .find(|(k, _)| k == slot_name)
                .map(|(_, handle)| *handle)
            {
                Some(handle) => handle,
                None => {
                    // A material has no depth to give: only the renderer's own
                    // buffer holds any, and a pass is what reads it. A
                    // material shader that asks for depth is a mistake worth
                    // naming rather than a slot quietly filled with white.
                    rendering_resource_storage
                        .default_texture_for(slot.texture_type)
                        .ok_or_else(|| RendererError::Other {
                            detail: format!(
                                "material `{name}` declares texture slot `{slot_name}` as depth, which only a pass can read"
                            ),
                        })?
                }
            };

            let texture = rendering_resource_storage
                .textures
                .get(renderer_texture_handle)
                .unwrap();
            check_slot_shape(
                &format!("material `{name}`"),
                slot_name,
                slot.texture_type,
                texture,
            )?;

            entries.push(wgpu::BindGroupEntry {
                binding: slot.texture_binding,
                resource: wgpu::BindingResource::TextureView(&texture.texture_view),
            });

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
