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

pub use material::{Material, MaterialBuilder, MaterialDocument, MaterialTexture};
pub use mesh::{Mesh, MeshImportSettings, MeshVertex};
pub use render_pass::RenderPass;
pub use render_pass::RenderPassDocument;
// Defined by the renderer contract (`pill_renderer_api::frame`), which a
// resolved pass is written in too; re-exported so `assets::` paths keep working.
pub use pill_renderer_api::frame::{CullMode, MaterialParameter, PassKind, PassTarget};
pub use rendering_pipeline::RenderingPipeline;
pub use shader::{parameter_slots_by_name, texture_slots_by_name};
pub use shader::{
    Shader, ShaderBuilder, ShaderParameterSlot, ShaderParameterType, ShaderTextureSlot,
};
pub use texture::{Texture, TextureImportSettings, TextureType};

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

#[cfg(test)]
mod tests {
    use super::*;

    /// The asset types' shared names are an identity contract: they are what
    /// lets the data module, the GPU module and a project reach one column per
    /// type, so the exact strings are asserted here.
    #[test]
    fn the_asset_types_keep_their_pinned_shared_names() {
        let names = [
            Mesh::shared_name(),
            Texture::shared_name(),
            Shader::shared_name(),
            Material::shared_name(),
            RenderPass::shared_name(),
            RenderingPipeline::shared_name(),
        ];
        assert_eq!(
            names,
            [
                Some("pill_master_renderer::assets::Mesh"),
                Some("pill_master_renderer::assets::Texture"),
                Some("pill_master_renderer::assets::Shader"),
                Some("pill_master_renderer::assets::Material"),
                Some("pill_master_renderer::assets::RenderPass"),
                Some("pill_master_renderer::assets::RenderingPipeline"),
            ]
        );
    }
}
