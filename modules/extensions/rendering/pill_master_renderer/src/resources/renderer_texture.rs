//! The GPU textures the renderer samples and renders into.
//!
//! # Responsibilities
//!
//! - Own the texture, view, and sampler triple behind every texture handle,
//!   so a resource that exists is complete ([`RendererTexture`]).
//! - Turn decoded RGBA bytes into a texture under the asset's name
//!   ([`RendererTexture::new_texture`]).
//! - Create offscreen colour targets at a caller-chosen format, and the
//!   surface-sized depth buffer ([`RendererTexture::new_render_target`],
//!   [`RendererTexture::new_depth_texture`]).
//!
//! # Design
//!
//! One type serves uploaded assets, offscreen passes, and depth because every
//! consumer needs the same three handles, and the constructors differ only in
//! how the texture is described. Every driver call is captured, so a refusal
//! arrives as an error naming the resource instead of a panic through wgpu's
//! uncaptured-error handler.

// Current crate
use crate::{
    assets::TextureType,
    error::{capturing_validation, Result},
};

// --- Handle ---

pill_core::define_slot_key!(RendererTextureHandle);

// --- Texture ---

/// A texture together with the view and sampler that make it usable.
///
/// One type covers every texture the renderer holds - uploaded assets,
/// offscreen colour targets, and depth - so a single handle carries all three
/// GPU objects a consumer needs: the view for binding, the sampler for
/// reading, and the texture the other two belong to.
pub struct RendererTexture {
    pub texture: wgpu::Texture,
    pub texture_view: wgpu::TextureView,
    pub sampler: wgpu::Sampler,
}

impl RendererTexture {
    /// Pixel format of the renderer's depth buffer.
    ///
    /// Pipeline state that renders into the buffer has to declare the same
    /// format the attachment was created with, so the value sits beside the
    /// constructor that creates it.
    pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

    /// Creates a texture, view, and sampler from decoded RGBA pixels.
    ///
    /// This is how project images become GPU resources: the bytes are
    /// uploaded at construction, so a failure is reported against the asset
    /// being loaded. `name` labels the texture and the errors that mention it.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::RendererError::Other`] naming the asset when
    /// `rgba` does not carry `width * height * 4` bytes, when `texture_type`
    /// is [`TextureType::Depth`] rather than an image, or when the driver
    /// refuses the texture, the upload, or the sampler.
    pub fn new_texture(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        name: Option<&str>,
        rgba: &[u8],
        width: u32,
        height: u32,
        texture_type: TextureType,
    ) -> Result<Self> {
        // A buffer that does not match the declared extent makes
        // `write_texture` fail validation, which would reach the
        // uncaptured-error handler as a panic instead of naming the asset;
        // check it here, where the name is still at hand.
        let expected_bytes = width as usize * height as usize * 4;
        if rgba.len() != expected_bytes {
            return Err(crate::error::RendererError::Other {
                detail: format!(
                    "texture `{}` declares {width}x{height} but carries {} bytes of RGBA data (expected {expected_bytes})",
                    name.unwrap_or("<unnamed>"),
                    rgba.len()
                ),
            });
        }

        // Get size
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };

        // Specify texture format
        let format = match texture_type {
            TextureType::Color => wgpu::TextureFormat::Rgba8UnormSrgb,
            TextureType::Normal => wgpu::TextureFormat::Rgba8Unorm,
            // A file has no way to carry depth, so an asset that says it does is
            // a mistake the caller should hear about rather than a texture with
            // the wrong channels.
            TextureType::Depth => {
                return Err(crate::error::RendererError::Other {
                    detail: format!(
                        "texture `{}` is loaded as depth, which only the renderer's own depth buffer can be",
                        name.unwrap_or("<unnamed>")
                    ),
                });
            }
        };

        // Create texture. Scoped: a driver refusal must arrive as a message
        // naming the asset, not as a panic through the uncaptured-error
        // handler.
        let texture = capturing_validation(device, || {
            device.create_texture(&wgpu::TextureDescriptor {
                label: name,
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        })
        .map_err(|detail| texture_failure(name, "creation", detail))?;

        // Write data to texture
        capturing_validation(device, || {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                rgba,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(4 * width),
                    rows_per_image: Some(height),
                },
                size,
            )
        })
        .map_err(|detail| texture_failure(name, "upload", detail))?;

        // Create texture view
        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        // Create sampler. Filtering is consistent in both directions and the
        // texture carries no mip levels, so nearest and linear only differ in
        // how a minified texel is chosen; linear is what magnification uses.
        let sampler = capturing_validation(device, || {
            device.create_sampler(&wgpu::SamplerDescriptor {
                address_mode_u: wgpu::AddressMode::Repeat,
                address_mode_v: wgpu::AddressMode::Repeat,
                address_mode_w: wgpu::AddressMode::Repeat,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                mipmap_filter: wgpu::FilterMode::Nearest,
                // Both clamps are required by wgpu's validation even when the
                // texture has a single mip level.
                lod_min_clamp: 0.0,
                lod_max_clamp: 100.0,
                ..Default::default()
            })
        })
        .map_err(|detail| texture_failure(name, "sampler", detail))?;

        Ok(Self {
            texture,
            texture_view,
            sampler,
        })
    }

    /// An offscreen colour target: written by one pass, read by the next.
    ///
    /// The caller picks the format for the same reason the surface format is
    /// picked from what the adapter offers: a chain that ends in tonemapping has
    /// to carry the range the scene produced between its steps, and a format
    /// that cannot hold it clips the picture on the way through.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::RendererError::Other`] naming the target when
    /// the driver refuses the texture or its sampler.
    pub fn new_render_target(
        device: &wgpu::Device,
        label: &str,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) -> Result<Self> {
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };

        let texture = capturing_validation(device, || {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
        })
        .map_err(|detail| crate::error::RendererError::Other {
            detail: format!("offscreen target `{label}` texture: {detail}"),
        })?;

        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        // Clamped and filtered: a post pass samples the frame it was handed at
        // whatever uv its own shader asks for, and wrapping there would fold the
        // opposite edge of the picture back into it.
        let sampler = capturing_validation(device, || {
            device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some(label),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                mipmap_filter: wgpu::FilterMode::Nearest,
                ..Default::default()
            })
        })
        .map_err(|detail| crate::error::RendererError::Other {
            detail: format!("offscreen target `{label}` sampler: {detail}"),
        })?;

        Ok(Self {
            texture,
            texture_view,
            sampler,
        })
    }

    /// Creates the surface-sized depth buffer, with a sampler for reading it.
    ///
    /// Taking the surface configuration rather than a size keeps the buffer
    /// and the swapchain in step: the renderer rebuilds the depth texture on
    /// every resize by passing the new configuration through here.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::RendererError::Other`] naming the buffer when
    /// the driver refuses the texture or its sampler.
    pub fn new_depth_texture(
        device: &wgpu::Device,
        surface_configuration: &wgpu::SurfaceConfiguration,
        label: &str,
    ) -> Result<Self> {
        // Get size
        let size = wgpu::Extent3d {
            // Depth texture needs to be the same size as window
            width: surface_configuration.width,
            height: surface_configuration.height,
            depth_or_array_layers: 1,
        };

        // Create texture
        let texture = capturing_validation(device, || {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: Self::DEPTH_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
        })
        .map_err(|detail| crate::error::RendererError::Other {
            detail: format!("depth texture `{label}`: {detail}"),
        })?;

        // Create texture view
        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        // A read sampler, not a comparison one: the pass path binds depth under
        // a `TextureSampleType::Depth` layout, and wgpu refuses a comparison
        // sampler for that. Clamped and nearest, so a lookup reads the depth of
        // the texel under the uv rather than blending across a depth edge.
        let sampler = capturing_validation(device, || {
            device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("depth_read_sampler"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Nearest,
                min_filter: wgpu::FilterMode::Nearest,
                mipmap_filter: wgpu::FilterMode::Nearest,
                lod_min_clamp: 0.0,
                lod_max_clamp: 100.0,
                ..Default::default()
            })
        })
        .map_err(|detail| crate::error::RendererError::Other {
            detail: format!("depth texture `{label}` sampler: {detail}"),
        })?;

        Ok(Self {
            texture,
            texture_view,
            sampler,
        })
    }
}

/// Attribute a captured wgpu failure to the texture that caused it.
fn texture_failure(name: Option<&str>, stage: &str, detail: String) -> crate::error::RendererError {
    crate::error::RendererError::Other {
        detail: format!(
            "texture `{}` failed at {stage}: {detail}",
            name.unwrap_or("<unnamed>")
        ),
    }
}
