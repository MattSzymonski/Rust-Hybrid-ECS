//! GPU-side storage for every resource the renderer holds.
//!
//! # Responsibilities
//!
//! - Own the slot map behind every GPU resource handle the renderer hands out.
//! - Install the stand-in textures a shader slot falls back to when a material
//!   or pass binds nothing.
//!
//! # Design
//!
//! One struct carries all five maps and the engine parameters, so asset sync,
//! pass building, and drawing reach every GPU resource through the same
//! reference. The stand-in textures are created at construction and their
//! handles kept, so a slot that binds nothing falls back to the same white,
//! flat-normal, or black environment texture.

// External crates
use pill_core::slot_map::SlotMap;

// Current crate
use crate::{
    assets::TextureType,
    error::Result,
    resources::{
        EngineParameters, RendererCamera, RendererCameraHandle, RendererMaterial,
        RendererMaterialHandle, RendererMesh, RendererMeshHandle, RendererShader,
        RendererShaderHandle, RendererTexture, RendererTextureHandle,
    },
};

/// Starting capacity of the shader slot map.
///
/// A capacity hint for the underlying vector, not a limit: the map grows past
/// it when a project registers more.
pub const INITIAL_SHADER_CAPACITY: usize = 10;
/// Starting capacity of the texture slot map.
pub const INITIAL_TEXTURE_CAPACITY: usize = 10;
/// Starting capacity of the material slot map.
pub const INITIAL_MATERIAL_CAPACITY: usize = 10;
/// Starting capacity of the mesh slot map.
pub const INITIAL_MESH_CAPACITY: usize = 10;
/// Starting capacity of the camera slot map.
pub const INITIAL_CAMERA_CAPACITY: usize = 10;

/// Every GPU resource the renderer holds, keyed by the handles it hands out.
///
/// One storage carries all five maps plus the engine parameters, so asset
/// sync, pass building, and drawing reach any resource through the same
/// borrow. The stand-in handles sit beside the maps that own their textures,
/// so a slot that binds nothing still resolves to one.
pub struct RendererResourceStorage {
    /// Shader pipelines and their bind group layouts, one per shader handle.
    pub(crate) shaders: SlotMap<RendererShaderHandle, RendererShader>,
    /// Material bind groups, one per material handle.
    pub(crate) materials: SlotMap<RendererMaterialHandle, RendererMaterial>,
    /// Uploaded textures, each with its view and sampler.
    pub(crate) textures: SlotMap<RendererTextureHandle, RendererTexture>,
    /// Mesh vertex and index buffers, ready to draw.
    pub(crate) meshes: SlotMap<RendererMeshHandle, RendererMesh>,
    /// Camera uniform buffers and the bind groups that carry them.
    pub(crate) cameras: SlotMap<RendererCameraHandle, RendererCamera>,
    /// The engine-wide uniform buffer and bind group, refilled every frame.
    pub(crate) engine_parameters: EngineParameters,
    /// The stand-in bound when a colour texture slot binds nothing.
    pub(crate) default_color_texture: RendererTextureHandle,
    /// The stand-in bound when a normal texture slot binds nothing.
    pub(crate) default_normal_texture: RendererTextureHandle,
    /// The stand-in bound when an equirect slot binds nothing: one black texel,
    /// so an unset sky is black rather than a white glare.
    pub(crate) default_equirect_texture: RendererTextureHandle,
    /// The stand-in bound when a cubemap slot binds nothing: six black faces.
    pub(crate) default_cubemap_texture: RendererTextureHandle,
}

impl RendererResourceStorage {
    /// Creates the storage with each map sized to its starting capacity, and
    /// installs the stand-in textures every texture slot can fall back to.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::RendererError::Other`] naming the stand-in when
    /// the driver refuses its texture, upload, or sampler: `default_color`,
    /// `default_normal`, `default_equirect` or `default_cubemap`. Everything
    /// else the constructor touches only records work and cannot fail.
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Result<Self> {
        let mut storage = RendererResourceStorage {
            shaders: SlotMap::<RendererShaderHandle, RendererShader>::with_capacity_and_key(
                INITIAL_SHADER_CAPACITY,
            ),
            textures: SlotMap::<RendererTextureHandle, RendererTexture>::with_capacity_and_key(
                INITIAL_TEXTURE_CAPACITY,
            ),
            materials: SlotMap::<RendererMaterialHandle, RendererMaterial>::with_capacity_and_key(
                INITIAL_MATERIAL_CAPACITY,
            ),
            meshes: SlotMap::<RendererMeshHandle, RendererMesh>::with_capacity_and_key(
                INITIAL_MESH_CAPACITY,
            ),
            cameras: SlotMap::<RendererCameraHandle, RendererCamera>::with_capacity_and_key(
                INITIAL_CAMERA_CAPACITY,
            ),
            engine_parameters: EngineParameters::new(device)?,
            default_color_texture: RendererTextureHandle::new(
                0,
                std::num::NonZeroU32::new(1).unwrap(),
            ),
            default_normal_texture: RendererTextureHandle::new(
                0,
                std::num::NonZeroU32::new(1).unwrap(),
            ),
            default_equirect_texture: RendererTextureHandle::new(
                0,
                std::num::NonZeroU32::new(1).unwrap(),
            ),
            default_cubemap_texture: RendererTextureHandle::new(
                0,
                std::num::NonZeroU32::new(1).unwrap(),
            ),
        };
        storage.install_default_textures(device, queue)?;
        Ok(storage)
    }

    /// The stand-in a slot of `texture_type` falls back to when it binds
    /// nothing, or `None` for depth, which only a pass's own input can supply.
    pub(crate) fn default_texture_for(
        &self,
        texture_type: TextureType,
    ) -> Option<RendererTextureHandle> {
        match texture_type {
            TextureType::Color => Some(self.default_color_texture),
            TextureType::Normal => Some(self.default_normal_texture),
            TextureType::Equirect => Some(self.default_equirect_texture),
            TextureType::Cubemap => Some(self.default_cubemap_texture),
            TextureType::Depth => None,
        }
    }

    /// Installs the stand-in textures a texture slot falls back to when it
    /// binds nothing: white for colour slots, the flat normal map for normal
    /// slots, black for equirect and cubemap slots.
    ///
    /// The colour and normal stand-ins are files the data module ships, read
    /// through its store; one that cannot be read falls back to the single
    /// texel built here, because a window without a colour texture is worse
    /// than a white one.
    ///
    /// Fails only when the driver refuses one of the textures.
    fn install_default_textures(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<()> {
        self.default_color_texture = self.textures.insert(Self::stand_in(
            device,
            queue,
            "default_color",
            Some(pill_master_renderer_data::DEFAULT_COLOR_TEXTURE),
            TextureType::Color,
            &[255, 255, 255, 255],
        )?);
        self.default_normal_texture = self.textures.insert(Self::stand_in(
            device,
            queue,
            "default_normal",
            Some(pill_master_renderer_data::DEFAULT_NORMAL_TEXTURE),
            TextureType::Normal,
            &[128, 128, 255, 255],
        )?);
        // Half-float zeros are zero bytes: one black texel, and six of them for
        // the cube's faces. Neither has a file form an importer reads, so the
        // two environment stand-ins stay texels built here.
        self.default_equirect_texture = self.textures.insert(Self::stand_in(
            device,
            queue,
            "default_equirect",
            None,
            TextureType::Equirect,
            &[0; 8],
        )?);
        self.default_cubemap_texture = self.textures.insert(Self::stand_in(
            device,
            queue,
            "default_cubemap",
            None,
            TextureType::Cubemap,
            &[0; 48],
        )?);
        Ok(())
    }

    /// One stand-in: the file the module ships when `path` names one it can
    /// read, else a single texel of `texel`.
    fn stand_in(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        name: &str,
        path: Option<&str>,
        texture_type: TextureType,
        texel: &[u8],
    ) -> Result<RendererTexture> {
        if let Some(path) = path {
            match pill_master_renderer_data::load_shipped_texture(path, texture_type) {
                Ok(texture) => {
                    return RendererTexture::new_texture(
                        device,
                        queue,
                        Some(name),
                        &texture.rgba,
                        texture.width,
                        texture.height,
                        texture.texture_type,
                    );
                }
                Err(error) => pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::RENDERING,
                    "stand-in texture `{path}` is not readable; using the built-in texel: {error}"
                ),
            }
        }
        RendererTexture::new_texture(device, queue, Some(name), texel, 1, 1, texture_type)
    }
}
