//! A GPU texture held as an engine resource.
//!
//! # Responsibilities
//!
//! - Declares [`GpuTexture`]: a texture, its default view, and its sampler,
//!   stored in the [`World`](pill_engine::world::World) as a single resource.
//! - Builds one from raw RGBA8 pixels ([`GpuTexture::from_rgba8`]) or as an
//!   empty surface of a given size ([`GpuTexture::new_empty`]).
//!
//! # Design
//!
//! A resource only has to be `Send + Sync + 'static`; it is not required to be
//! copyable, blittable or serializable. wgpu's handles are all three of those
//! things and reference-counted besides, so they satisfy the bound directly and
//! the engine stores them like any other value - `ErasedResource` records a
//! real `drop_in_place::<GpuTexture>`, so the handles are released when the
//! resource is replaced or the world goes away.
//!
//! Resources are singletons keyed by type, so exactly one `GpuTexture` exists
//! at a time. A pipeline needing many textures wraps a collection in one
//! resource of its own rather than declaring a second texture type.
//!
//! ## Why creation takes a device rather than making one
//!
//! `wgpu::Texture` can only come from a `wgpu::Device`, and the device belongs
//! to [`Renderer`](crate::renderer::Renderer), which the *host* owns beside the
//! engine rather than inside it. So the constructors here take `&Device` and
//! `&Queue`, and the caller - which already holds a renderer - inserts the
//! result with
//! [`World::insert_resource`](pill_engine::world::World::insert_resource).
//! The engine stores and owns the texture; it does not manufacture it, because
//! it has no GPU to manufacture it from.
//!
//! ## Hot reload
//!
//! This crate is linked into the host, not hot-loaded, so the destructor stored
//! with the resource lives in an image that never unloads. That is what makes
//! it safe for a resource to own GPU handles at all; see
//! [`World::rehome_resources`](pill_engine::world::World::rehome_resources)
//! for the constraint a hot-loadable module would face instead.
//!
//! ## Usage
//!
//! ```no_run
//! # use pill_master_renderer::texture::GpuTexture;
//! # fn demo(engine: &mut pill_engine::Engine, device: &wgpu::Device, queue: &wgpu::Queue) {
//! let pixels = [255u8, 0, 0, 255]; // one opaque red texel
//! let texture = GpuTexture::from_rgba8(device, queue, "tile", &pixels, 1, 1);
//! engine.world_mut().insert_resource(texture);
//!
//! // Later, anywhere holding the engine:
//! let stored = engine.world().get_resource::<GpuTexture>().unwrap();
//! let _ = &stored.texture_view;
//! # }
//! ```

// External crates
use pill_engine::Resource;

// =============================================================================
// GpuTexture
// =============================================================================

/// One GPU texture, its default view, and the sampler reading it.
///
/// Stored in the world as a resource. The three handles travel together
/// because a bind group needs all three and they share a lifetime: a view
/// outliving its texture, or a sampler paired with the wrong one, is a bug
/// this grouping makes unrepresentable.
pub struct GpuTexture {
    /// The texture allocation itself.
    pub texture: wgpu::Texture,
    /// Default whole-texture view, the form a bind group consumes.
    pub texture_view: wgpu::TextureView,
    /// Sampler describing how shaders filter and wrap this texture.
    pub sampler: wgpu::Sampler,
}

// A resource need only be `Send + Sync + 'static`. wgpu's handles are all
// three, so nothing beyond the marker is required.
impl Resource for GpuTexture {}

impl GpuTexture {
    /// The format every texture here uses: 8-bit RGBA, sRGB-encoded.
    ///
    /// Fixed rather than a parameter because the sprite pipeline samples in
    /// sRGB and a mismatch shows up as washed-out color rather than an error.
    pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

    /// Allocate a texture of `width` x `height` with no pixel data written.
    ///
    /// Usable as a render target or as a surface filled later through
    /// [`Self::write_rgba8`]. Both dimensions are clamped to at least 1, since
    /// wgpu rejects a zero extent and a caller deriving a size from a window
    /// can legitimately arrive here with one during a minimize.
    pub fn new_empty(device: &wgpu::Device, label: &str, width: u32, height: u32) -> Self {
        let size = wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        };

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: Self::FORMAT,
            // `TEXTURE_BINDING` to sample it, `COPY_DST` so `write_rgba8` can
            // fill it, `RENDER_ATTACHMENT` so it can also serve as a target.
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });

        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some(label),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            // Nearest filtering: sprites are pixel art until something asks
            // otherwise, and linear filtering on a sprite atlas bleeds
            // neighbouring tiles into each other at the seams.
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        Self {
            texture,
            texture_view,
            sampler,
        }
    }

    /// Allocate a texture and upload `pixels` into it.
    ///
    /// `pixels` is tightly packed RGBA8, four bytes per texel, `width * height`
    /// texels in row-major order.
    ///
    /// # Panics
    ///
    /// Panics when `pixels` is not exactly `width * height * 4` bytes. A
    /// short buffer would otherwise upload adjacent memory, and a long one
    /// means the caller and this function disagree about the layout - both are
    /// programmer errors at a call site that knows its own image, not runtime
    /// conditions worth threading a `Result` through the setup path for.
    pub fn from_rgba8(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        label: &str,
        pixels: &[u8],
        width: u32,
        height: u32,
    ) -> Self {
        let texture = Self::new_empty(device, label, width, height);
        texture.write_rgba8(queue, pixels);
        texture
    }

    /// Overwrite the whole texture with `pixels`.
    ///
    /// The upload is queued, not immediate: wgpu performs it before the next
    /// submitted command buffer reads the texture.
    ///
    /// # Panics
    ///
    /// Panics when `pixels` is not exactly `width * height * 4` bytes; see
    /// [`Self::from_rgba8`] for why this is a panic.
    pub fn write_rgba8(&self, queue: &wgpu::Queue, pixels: &[u8]) {
        let size = self.texture.size();
        let expected = size.width as usize * size.height as usize * 4;
        assert_eq!(
            pixels.len(),
            expected,
            "RGBA8 upload for a {}x{} texture needs {expected} bytes, got {}",
            size.width,
            size.height,
            pixels.len(),
        );

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * size.width),
                rows_per_image: Some(size.height),
            },
            size,
        );
    }

    /// Width of the texture in texels.
    pub fn width(&self) -> u32 {
        self.texture.size().width
    }

    /// Height of the texture in texels.
    pub fn height(&self) -> u32 {
        self.texture.size().height
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The resource bound is the only thing the engine asks of a stored type,
    /// and wgpu's handles satisfy it - which is what lets `GpuTexture` be a
    /// resource at all. Asserted at compile time; no GPU is involved.
    #[test]
    fn gpu_texture_satisfies_the_resource_bound() {
        fn assert_resource<T: Resource>() {}
        assert_resource::<GpuTexture>();
    }

    /// A resource is identified by `TypeId`, not by a declared name. A GPU
    /// handle means nothing to another artifact or to C#, so claiming a shared
    /// name would invite exactly the cross-boundary reads that cannot work.
    #[test]
    fn gpu_texture_is_not_shared_across_artifacts() {
        assert!(GpuTexture::shared_name().is_none());
    }
}
