use crate::resources::{
    EngineParameters, RendererCamera, RendererMaterial, RendererMesh, RendererShader,
    RendererTexture,
};

use crate::{
    error::Result,
    slot_map::{
        RendererCameraHandle, RendererMaterialHandle, RendererMeshHandle, RendererShaderHandle,
        RendererTextureHandle, SlotMap,
    },
};

pub const MAX_SHADERS: usize = 10;
pub const MAX_TEXTURES: usize = 10;
pub const MAX_MATERIALS: usize = 10;
pub const MAX_MESHES: usize = 10;
pub const MAX_CAMERAS: usize = 10;

pub struct RendererResourceStorage {
    pub(crate) shaders: SlotMap<RendererShaderHandle, RendererShader>,
    pub(crate) materials: SlotMap<RendererMaterialHandle, RendererMaterial>,
    pub(crate) textures: SlotMap<RendererTextureHandle, RendererTexture>,
    pub(crate) meshes: SlotMap<RendererMeshHandle, RendererMesh>,
    pub(crate) cameras: SlotMap<RendererCameraHandle, RendererCamera>,
    pub(crate) engine_parameters: EngineParameters,
    pub(crate) default_color_texture: RendererTextureHandle,
    pub(crate) default_normal_texture: RendererTextureHandle,
    /// A sampler for reading depth as a value rather than comparing against it.
    ///
    /// The depth buffer carries a comparison sampler for shadow tests, and wgpu
    /// will not accept that where a layout asks for a plain read - so a pass that
    /// reconstructs position from depth gets this one.
    pub(crate) depth_sampler: wgpu::Sampler,
}

impl RendererResourceStorage {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Result<Self> {
        let mut storage = RendererResourceStorage {
            shaders: SlotMap::<RendererShaderHandle, RendererShader>::with_capacity_and_key(
                MAX_SHADERS,
            ),
            textures: SlotMap::<RendererTextureHandle, RendererTexture>::with_capacity_and_key(
                MAX_TEXTURES,
            ),
            materials: SlotMap::<RendererMaterialHandle, RendererMaterial>::with_capacity_and_key(
                MAX_MATERIALS,
            ),
            meshes: SlotMap::<RendererMeshHandle, RendererMesh>::with_capacity_and_key(MAX_MESHES),
            cameras: SlotMap::<RendererCameraHandle, RendererCamera>::with_capacity_and_key(
                MAX_CAMERAS,
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
            depth_sampler: device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("depth_read_sampler"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Nearest,
                min_filter: wgpu::FilterMode::Nearest,
                mipmap_filter: wgpu::FilterMode::Nearest,
                ..Default::default()
            }),
        };
        storage.install_default_textures(device, queue)?;
        Ok(storage)
    }

    pub fn clear_assets(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) -> Result<()> {
        self.materials.clear();
        self.meshes.clear();
        self.textures.clear();
        self.shaders.clear();
        self.install_default_textures(device, queue)
    }

    fn install_default_textures(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<()> {
        self.default_color_texture = self.textures.insert(RendererTexture::new_texture(
            device,
            queue,
            Some("default_color"),
            &[255, 255, 255, 255],
            1,
            1,
            crate::assets::TextureType::Color,
        )?);
        self.default_normal_texture = self.textures.insert(RendererTexture::new_texture(
            device,
            queue,
            Some("default_normal"),
            &[128, 128, 255, 255],
            1,
            1,
            crate::assets::TextureType::Normal,
        )?);
        Ok(())
    }
}
