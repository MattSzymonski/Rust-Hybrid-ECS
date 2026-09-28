//! Game-facing renderer assets stored in the world's `AssetManager`.
//!
//! # Responsibilities
//!
//! - Own the asset types a project draws from - [`Mesh`], [`Texture`],
//!   [`Shader`], [`Material`], [`RenderPass`], [`RenderingPipeline`] -
//!   re-exported flat so a game names them without the submodule they sit in.
//! - Carry each type's builder and binding declarations: [`MaterialBuilder`],
//!   [`ShaderBuilder`], the parameter and texture slot types, and the
//!   `pub(crate)` helpers that key slot lists by the names they carry.
//! - Register every type behind `dyn Asset`, so the world's `AssetManager`
//!   stores them many per type like any other asset.
//! - Provide [`asset_key`], packing a handle's index and generation into the
//!   `u64` the renderer's caches key on.
//!
//! # Design
//!
//! Each type here is data plus declaration: a game builds it, the
//! `AssetManager` owns it, and the renderer reads it while resolving a frame,
//! with the GPU side - caches, pipelines, bind groups - living under
//! [`crate::resources`] and [`crate::renderer`]. [`asset_key`] packs a
//! handle's generation above its index, so one `u64` tells a cache which
//! asset and which version of it a resource was built for.

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
pub use shader::{parameter_slots_by_name, texture_slots_by_name};
pub use shader::{
    Shader, ShaderBuilder, ShaderParameterSlot, ShaderParameterType, ShaderTextureSlot,
};
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
