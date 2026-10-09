//! Material assets connecting shaders, textures, and uniform parameters.
//!
//! # Responsibilities
//!
//! - Carry what one drawn surface needs: the shader it draws with and the
//!   values that shader reads, both as uniform parameters and as bound
//!   textures.
//! - Keep materials data. A material names its shader by handle and its values
//!   by the slot names the shader declares, so a game describes an appearance
//!   without reaching into the renderer.
//! - Load a material from a `.material` file in `res`
//!   ([`StandaloneAsset`] with [`MaterialDocument`]), which names its shader
//!   and textures by guid so the file still resolves in the next run.
//!
//! # Design
//!
//! The fields mirror [`RenderPass`](crate::RenderPass) on purpose: a shader
//! handle, a parameter map and a texture map, keyed by the slot names the
//! shader declares and packed by the same rules. The renderer therefore drives
//! a material and a pass through one code path, and a material's
//! `rendering_order` decides where it sorts within the pass that draws it.

use std::collections::BTreeMap;

use pill_engine::{
    pill_mirror_impl, pill_mirror_method, pill_mirror_object, Asset, AssetLoadResult, AssetManager,
    AssetReference, Handle, StandaloneAsset,
};
use serde::{Deserialize, Serialize};

use super::{Shader, Texture};
use crate::config::pbr_pipeline;
// Part of the frame contract (a resolved pass carries parameters too).
use pill_renderer_api::frame::MaterialParameter;

/// A texture handle bound to one of a material's slots.
///
/// Wraps the handle rather than a GPU texture: the renderer resolves it when
/// the material is built, so the same asset can back any number of slots
/// without being copied.
#[derive(Clone, Debug)]
pub struct MaterialTexture {
    /// Texture asset this binding resolves through.
    pub texture: Handle<Texture>,
}

/// A surface appearance: the shader a draw runs and the values that shader
/// reads.
///
/// A material is an asset, so the renderer rebuilds its bind groups when the
/// asset's version moves rather than expecting a game to mutate renderer state
/// mid frame. [`MaterialBuilder`] is the supported way to make one.
///
/// Its maps are `BTreeMap`s, never `HashMap`s. An empty `HashMap` does not
/// allocate: it points at a static inside the binary that created it. This
/// value lives in the world and outlives that binary - a project or module
/// that built it is reloaded and its retired image eventually unmapped - and
/// reading such a map afterwards faults. A `BTreeMap` holds no such pointer.
#[derive(Clone, Debug)]
#[pill_mirror_object(asset, standalone)]
pub struct Material {
    /// Label used in logs, profiling and error messages.
    pub name: String,
    /// Shader the material draws with. [`Handle::INVALID`] selects the
    /// renderer's built-in shader, as does a handle that names no loaded
    /// shader.
    pub shader: Handle<Shader>,
    /// Textures bound to the slots the shader declares, by slot name.
    ///
    /// A map, like [`RenderPass::textures`](crate::RenderPass), so a slot named
    /// twice has one entry rather than a vector the binder reads only its first
    /// match out of. A declared slot the material leaves unbound falls back to
    /// the renderer's default texture for that slot's type.
    pub textures: BTreeMap<String, MaterialTexture>,
    /// Uniform parameters, packed one 16-byte slot each, in the order the
    /// shader declares them. A declared slot the material leaves unset packs
    /// as zero.
    pub parameters: BTreeMap<String, MaterialParameter>,
    /// Sort key for materials inside the pass that draws them.
    ///
    /// The queue's composed key stores this byte inverted and sorts the key
    /// ascending, so a *larger* value is drawn earlier: the default `u8::MAX`
    /// draws before any material given an explicit order. Note the split with
    /// passes, where it is the opposite - a lower [`RenderPass::order`] runs
    /// first.
    pub rendering_order: u8,
}

/// Builds a [`Material`] one field at a time.
///
/// A fresh builder starts on the renderer's built-in shader with no textures,
/// no parameters, and the highest rendering order, so a material only mentions
/// the fields the game wants to differ from those defaults.
#[pill_mirror_object]
pub struct MaterialBuilder {
    material: Material,
}

#[pill_mirror_impl]
impl Material {
    /// Start building a material under the given name.
    ///
    /// The name labels the material in logs, profiling and error messages,
    /// which is why it is the one field the builder asks for up front.
    #[pill_mirror_method]
    pub fn builder(name: impl Into<String>) -> MaterialBuilder {
        MaterialBuilder {
            material: Self {
                name: name.into(),
                shader: Handle::INVALID,
                textures: BTreeMap::new(),
                parameters: BTreeMap::new(),
                rendering_order: u8::MAX,
            },
        }
    }
}

#[pill_mirror_impl]
impl MaterialBuilder {
    /// Set the shader the material draws with. [`Handle::INVALID`] keeps the
    /// renderer's built-in shader.
    #[pill_mirror_method]
    pub fn shader(mut self, shader: &Handle<Shader>) -> Self {
        self.material.shader = *shader;
        self
    }

    /// Bind a texture to one of the shader's declared slots.
    ///
    /// Binding a slot twice replaces the earlier binding: the map keeps one
    /// texture per slot, and the last call is the one that was meant.
    #[pill_mirror_method]
    pub fn texture(mut self, slot: impl Into<String>, texture: &Handle<Texture>) -> Self {
        self.material
            .textures
            .insert(slot.into(), MaterialTexture { texture: *texture });
        self
    }

    /// Set one scalar uniform parameter, under the slot the shader declares.
    #[pill_mirror_method]
    pub fn scalar_parameter(mut self, slot: impl Into<String>, value: f32) -> Self {
        self.material
            .parameters
            .insert(slot.into(), MaterialParameter::Scalar(value));
        self
    }

    /// Set one boolean uniform parameter, packed as a `u32` of 0 or 1.
    #[pill_mirror_method]
    pub fn bool_parameter(mut self, slot: impl Into<String>, value: bool) -> Self {
        self.material
            .parameters
            .insert(slot.into(), MaterialParameter::Bool(value));
        self
    }

    /// Set one colour uniform parameter, packed as three `f32`s.
    #[pill_mirror_method]
    pub fn color_parameter(mut self, slot: impl Into<String>, value: [f32; 3]) -> Self {
        self.material
            .parameters
            .insert(slot.into(), MaterialParameter::Color(value));
        self
    }

    /// Choose when the material draws relative to the others in the same pass.
    ///
    /// See [`Material::rendering_order`]: a larger value is drawn earlier.
    #[pill_mirror_method]
    pub fn rendering_order(mut self, value: u8) -> Self {
        self.material.rendering_order = value;
        self
    }

    /// Finish building and return the material.
    #[pill_mirror_method]
    pub fn build(self) -> Material {
        self.material
    }
}

/// A material as written in its `.material` file.
///
/// The asset itself, under the standard asset header (format version, asset
/// type, guid). Its shader and textures are guids
/// ([`AssetReference`]), resolved to handles when the file is loaded: a
/// handle means nothing in the next run, a guid does. A reference that names
/// no loaded asset resolves to [`Handle::INVALID`], which draws with the
/// renderer's built-in shader or the slot's default texture.
///
/// The default document is what a newly created file holds, and it draws as
/// it is: the PBR chain's shader (by its fixed guid,
/// [`pbr_pipeline::SHADER_GUID`](crate::config::pbr_pipeline::SHADER_GUID)) at
/// its neutral parameters, with no maps bound. It names the chain's shader
/// rather than leaving the shader unset because the chain is every project's
/// default frame, and its geometry pass draws only the instances that use its
/// own shader: a material on the renderer's built-in shader would load and
/// never be drawn there. Maps are `BTreeMap`s for the reason [`Material`]
/// gives.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MaterialDocument {
    /// The shader the material draws with; unset for the renderer's built-in
    /// one, which only a pass with no shader of its own draws.
    pub shader: AssetReference<Shader>,
    /// Textures bound to the shader's slots, by slot name.
    pub textures: BTreeMap<String, AssetReference<Texture>>,
    /// Uniform parameters, by slot name.
    pub parameters: BTreeMap<String, MaterialParameter>,
    /// See [`Material::rendering_order`].
    pub rendering_order: u8,
}

impl Default for MaterialDocument {
    /// The PBR chain's shader at its neutral parameters; see the type's docs.
    fn default() -> Self {
        Self {
            shader: AssetReference::new(pbr_pipeline::SHADER_GUID),
            textures: BTreeMap::new(),
            parameters: pbr_pipeline::neutral_parameters(),
            rendering_order: u8::MAX,
        }
    }
}

impl StandaloneAsset for Material {
    type Document = MaterialDocument;
    const FILE_EXTENSION: &'static str = "material";

    fn from_document(
        name: &str,
        document: MaterialDocument,
        assets: &AssetManager,
    ) -> AssetLoadResult<Self> {
        let textures = document
            .textures
            .into_iter()
            .map(|(slot, texture)| {
                let texture = texture.resolve(assets);
                (slot, MaterialTexture { texture })
            })
            .collect();
        Ok(Self {
            name: name.to_owned(),
            shader: document.shader.resolve(assets),
            textures,
            parameters: document.parameters,
            rendering_order: document.rendering_order,
        })
    }
}

/// The pinned shared name of [`Material`]; see the comment on its `Asset` impl.
const MATERIAL_SHARED_NAME: &str = "pill_master_renderer::assets::Material";

// Shared across binaries: the data module, the GPU module and every project
// compile their own copy of this crate, each with its own `TypeId`. The pinned
// name makes them one asset column (see `Asset::shared_name`); keep it
// verbatim when moving the type.
impl Asset for Material {
    fn shared_name() -> Option<&'static str> {
        Some(MATERIAL_SHARED_NAME)
    }

    fn shared_identity() -> Option<u128> {
        // A `const`, so the name is hashed at compile time. The default hashes
        // it on every call, and every `AssetManager` lookup makes that call:
        // the renderer does it several times per drawn entity, every frame.
        const IDENTITY: u128 =
            pill_engine::component::shared_component_identity(MATERIAL_SHARED_NAME);
        Some(IDENTITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A new `.material` file's document draws in the default frame: it
    /// names the PBR chain's shader, which resolves once the chain is
    /// installed, at the chain's neutral parameters, with nothing bound.
    #[test]
    fn the_default_document_is_the_pbr_chains_neutral_material() {
        let mut assets = AssetManager::new();
        pbr_pipeline::install(&mut assets).unwrap();
        let shader = assets
            .handle_by_name::<Shader>(pbr_pipeline::SHADER_NAME)
            .unwrap();

        let material =
            Material::from_document("materials/a.material", MaterialDocument::default(), &assets)
                .unwrap();

        assert_eq!(material.shader, shader);
        assert!(material.textures.is_empty());
        assert_eq!(material.parameters, pbr_pipeline::neutral_parameters());
        assert_eq!(material.rendering_order, u8::MAX);
        // The chain's own default material starts from the same values.
        let chain_default = assets
            .get_by_name::<Material>(pbr_pipeline::MATERIAL_NAME)
            .unwrap();
        assert_eq!(chain_default.parameters, material.parameters);
        assert_eq!(
            serde_json::to_value(MaterialDocument::default()).unwrap()["shader"],
            serde_json::json!(pbr_pipeline::SHADER_GUID.to_string())
        );
    }
}
