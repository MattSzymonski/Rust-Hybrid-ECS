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
use pill_core::debug;

// Current crate
use crate::{
    assets::MaterialParameter,
    error::{RendererError, Result},
    resources::{
        shader_bindings, RendererResourceStorage, RendererShaderHandle, RendererTextureHandle,
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

        // A material has no source of textures beyond the store, so the shared
        // builder's extra resolver never answers.
        let (parameters_bind_group, textures_bind_group) = shader_bindings::shader_bind_groups(
            device,
            queue,
            rendering_resource_storage,
            &format!("material `{name}`"),
            shader,
            parameters,
            textures,
            |_, _| Ok(None),
        )?;

        let renderer_material = Self {
            name: name.to_string(),
            shader_handle,
            parameters_bind_group,
            textures_bind_group,
        };

        // One line per material, hidden unless `engine::rendering` is at debug.
        let defaulted_textures = shader
            .texture_slots
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
}
