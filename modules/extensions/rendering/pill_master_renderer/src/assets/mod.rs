//! Game-facing renderer assets stored in the world's `AssetManager`.

mod material;
mod mesh;
mod shader;
mod texture;

pub use material::{Material, MaterialBuilder, MaterialParameter, MaterialTexture};
pub use mesh::{Mesh, MeshVertex};
pub use shader::{Shader, ShaderParameterSlot, ShaderParameterType, ShaderTextureSlot};
pub use texture::{Texture, TextureType};

use pill_engine::{Asset, Handle};

trait_type_map::impl_trait_accessible!(dyn Asset; Mesh, Texture, Shader, Material);

/// Packs a typed generational asset handle into a renderer cache key.
pub fn asset_key<T: Asset>(handle: Handle<T>) -> u64 {
    (u64::from(handle.generation()) << 32) | u64::from(handle.index())
}
