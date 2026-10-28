//! The bind groups a shader's declared slots resolve to, built once for every
//! owner that has some.
//!
//! # Responsibilities
//!
//! - Pack an owner's parameters into the uniform buffer its shader reads
//!   ([`shader_bind_groups`]).
//! - Resolve each texture slot the shader declares - the owner's own committed
//!   texture, a source the caller resolves, or the renderer's stand-in - into
//!   one textures bind group.
//!
//! # Design
//!
//! A material and a pass are the same thing to a shader: a set of parameter
//! values and a texture per declared slot. They differ only in where an extra
//! texture can come from - a pass reads the frame an earlier pass wrote, a
//! material has nothing beyond the store - so that difference is one closure and
//! everything else lives here. A change to how a slot binds therefore lands in
//! one file, and a shader's slots are walked the same way whichever owner asked.
//!
//! Both groups follow the shader's declaration rather than the owner's maps: a
//! value or texture named for a slot the shader never declares is ignored, and a
//! declared slot left unresolved falls back to the renderer's stand-in for its
//! type. Depth is refused rather than stood in for: the renderer's own depth
//! buffer is the only one there is, and a slot that asks for depth has to name
//! the source it means.

// Standard library
use std::collections::BTreeMap;

// External crates
use indexmap::IndexMap;
use pill_renderer_api::frame::MaterialParameter;

// Current crate
use crate::{
    assets::{ShaderParameterSlot, ShaderParameterType, ShaderTextureSlot},
    error::{capturing_validation, ErrorContext, RendererError, Result},
    resources::{check_slot_shape, RendererResourceStorage, RendererShader, RendererTextureHandle},
};

/// The uniform buffer size a shader's parameter slots need.
///
/// Every slot is one `vec4`-aligned 16-byte region in WGSL, so the size is the
/// slot count times 16. A material and a pass both size their buffers through
/// this, which is what stops them drifting apart on what they promise a shader.
pub(crate) fn uniform_size(parameter_slots: &IndexMap<String, ShaderParameterSlot>) -> usize {
    parameter_slots.len() * 16
}

/// Packs an owner's parameter values into the uniform buffer its shader reads.
///
/// Walks the shader's slots in declaration order and writes one 16-byte region
/// per slot, so a value is placed by the name the shader gave it rather than by
/// position in the map. A declared slot with no value packs as zero, which keeps
/// a partially filled material drawing.
pub(crate) fn write_parameters(
    queue: &wgpu::Queue,
    buffer: &wgpu::Buffer,
    parameter_slots: &IndexMap<String, ShaderParameterSlot>,
    parameters: &BTreeMap<String, MaterialParameter>,
) {
    let mut data = Vec::new();

    // Each parameter takes a full 16-byte slot (vec4 alignment in WGSL), padding
    // included. Packing by type would use less buffer space, but the fixed slot
    // is simpler and keeps a material and a pass packed identically.
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
}

/// Builds the parameters and textures bind groups one owner's shader reads.
///
/// `owner` names the pass or material in every message and every debug label, so
/// a refused slot says which thing asked for it. `resolve_extra` is the caller's
/// own source of textures - a pass's inputs, a material's nothing - and runs
/// before the committed textures, so an input a pass names wins over an asset of
/// the same slot name.
///
/// # Errors
///
/// Returns a [`RendererError`](crate::RendererError) when a slot cannot be
/// resolved - a source the caller could not produce, a depth slot with no
/// source, a texture that is not loaded - or when wgpu refuses a group. Creation
/// is scoped, so a refusal names the owner instead of panicking through the
/// uncaptured-error handler.
#[allow(clippy::too_many_arguments)]
pub(crate) fn shader_bind_groups<'views>(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    storage: &RendererResourceStorage,
    owner: &str,
    shader: &RendererShader,
    parameters: &BTreeMap<String, MaterialParameter>,
    textures: &[(String, RendererTextureHandle)],
    resolve_extra: impl Fn(
        &str,
        &ShaderTextureSlot,
    ) -> Result<Option<(&'views wgpu::TextureView, &'views wgpu::Sampler)>>,
) -> Result<(Option<wgpu::BindGroup>, Option<wgpu::BindGroup>)> {
    let parameters_bind_group = if shader.parameter_slots.is_empty() {
        None
    } else {
        // Not stored: a bind group holds what it references, so the buffer lives
        // as long as the group that reads it.
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(&format!("{owner}_parameters_buffer")),
            size: uniform_size(&shader.parameter_slots) as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        write_parameters(queue, &buffer, &shader.parameter_slots, parameters);
        let layout = shader
            .parameters_bind_group_layout
            .as_ref()
            .context("a shader with parameter slots has a parameters layout")?;
        let bind_group = capturing_validation(device, || {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(&format!("{owner}_parameters_bind_group")),
                layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0, // (set = 2, binding = 0)
                    resource: buffer.as_entire_binding(),
                }],
            })
        })
        .map_err(|detail| RendererError::Other { detail })?;
        Some(bind_group)
    };

    let textures_bind_group = if shader.texture_slots.is_empty() {
        None
    } else {
        let mut entries = Vec::new();
        for (slot_name, slot) in &shader.texture_slots {
            let source = match resolve_extra(slot_name, slot)? {
                Some(bound) => Some(bound),
                None => match textures.iter().find(|(name, _)| name == slot_name) {
                    Some((_, handle)) => {
                        let texture = storage.textures.get(*handle).context(format!(
                            "{owner} binds `{slot_name}` to a texture that is not loaded"
                        ))?;
                        check_slot_shape(owner, slot_name, slot.kind, texture)?;
                        Some((&texture.texture_view, &texture.sampler))
                    }
                    // The shader declared the slot and the owner named nothing
                    // for it, so it gets the renderer's stand-in for that kind
                    // of map. A depth slot has none: the renderer's own buffer is
                    // not a stand-in for a colour map, and only the owner can say
                    // which depth it meant.
                    None => {
                        let handle = storage.default_texture_for(slot.kind).ok_or_else(
                            || RendererError::Other {
                                detail: format!(
                                    "{owner} declares texture slot `{slot_name}` as depth, which has no stand-in"
                                ),
                            },
                        )?;
                        let texture = storage
                            .textures
                            .get(handle)
                            .context("the renderer's default textures exist")?;
                        Some((&texture.texture_view, &texture.sampler))
                    }
                },
            };
            let Some((view, sampler)) = source else {
                continue;
            };
            entries.push(wgpu::BindGroupEntry {
                binding: slot.texture_binding,
                resource: wgpu::BindingResource::TextureView(view),
            });
            entries.push(wgpu::BindGroupEntry {
                binding: slot.sampler_binding,
                resource: wgpu::BindingResource::Sampler(sampler),
            });
        }
        let layout = shader
            .textures_bind_group_layout
            .as_ref()
            .context("a shader with texture slots has a textures layout")?;
        let bind_group = capturing_validation(device, || {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(&format!("{owner}_textures_bind_group")),
                layout,
                entries: &entries,
            })
        })
        .map_err(|detail| RendererError::Other { detail })?;
        Some(bind_group)
    };

    Ok((parameters_bind_group, textures_bind_group))
}
