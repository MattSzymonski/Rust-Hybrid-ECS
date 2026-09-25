//! Game-facing renderer assets stored in the world's `AssetManager`.

mod material;
mod mesh;
mod render_pass;
mod rendering_pipeline;
mod shader;
mod texture;

pub use material::{Material, MaterialBuilder, MaterialParameter, MaterialTexture};
pub use mesh::{Mesh, MeshVertex};
pub use render_pass::{CullMode, PassKind, PassTarget, RenderPass};
pub use rendering_pipeline::RenderingPipeline;
pub use shader::{Shader, ShaderParameterSlot, ShaderParameterType, ShaderTextureSlot};
pub use texture::{Texture, TextureType};

use pill_engine::{Asset, Handle};

trait_type_map::impl_trait_accessible!(
    dyn Asset;
    Mesh,
    Texture,
    Shader,
    Material,
    RenderPass,
    RenderingPipeline
);

/// Packs a typed generational asset handle into a renderer cache key.
pub fn asset_key<T: Asset>(handle: Handle<T>) -> u64 {
    (u64::from(handle.generation()) << 32) | u64::from(handle.index())
}
