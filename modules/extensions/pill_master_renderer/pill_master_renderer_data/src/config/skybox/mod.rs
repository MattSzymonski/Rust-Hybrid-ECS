//! The skybox: the two shaders a sky material draws through, and the pass that
//! draws it.
//!
//! # Responsibilities
//!
//! - Install the equirect and cubemap skybox shaders under fixed names and
//!   guids ([`install_shaders`]), so a `.material` file can name them.
//! - Build the pass that draws a sky material behind the scene
//!   ([`skybox_pass`]), and say what a sky material's parameters mean
//!   ([`default_parameters`]).
//!
//! # Design
//!
//! A sky is a material, not a special asset: an ordinary `.material` file whose
//! shader is one of the two here and whose `sky` slot holds an
//! [`Equirect`](crate::TextureType::Equirect) or a
//! [`Cubemap`](crate::TextureType::Cubemap) texture. The pass is an ordinary
//! [`RenderPass`] of kind [`PassKind::Skybox`] that names the material: one
//! fullscreen triangle at the far plane, depth-tested against what the
//! geometry pass drew, so the sky shows only where no mesh does.
//!
//! The equirect shader's guid is `5628c9e4d9278fec79998a7adab65bc3`
//! ([`EQUIRECT_SHADER_GUID`]) and the cubemap one's
//! `a6dfb611b11dfb57c5039dd8b6fc1a46` ([`CUBEMAP_SHADER_GUID`]); a sky
//! material file names one of them:
//!
//! ```json
//! "asset": {
//!   "shader": "5628c9e4d9278fec79998a7adab65bc3",
//!   "textures": { "sky": "<guid of an .hdr imported as Equirect>" },
//!   "parameters": {
//!     "skybox_tint": { "Color": [1.0, 1.0, 1.0] },
//!     "skybox_exposure": { "Scalar": 1.0 },
//!     "skybox_rotation": { "Scalar": 0.0 }
//!   }
//! }
//! ```
//!
//! The shaders read the camera's inverse view-projection to turn each pixel
//! into the direction it looks along, and the equirect one maps that direction
//! to a uv with [`equirect_uv`](crate::assets::equirect_uv)'s convention.

// Standard library
use std::collections::BTreeMap;

// External crates
use pill_engine::{AssetGuid, AssetLoadResult, AssetManager, Handle};

// Current crate
use crate::assets::{
    Material, MaterialParameter, PassKind, PassTarget, RenderPass, Shader, ShaderParameterSlot,
    ShaderParameterType, ShaderTextureSlot, TextureType,
};
use crate::config::{ConfigShaderFile, ShaderSourceRecord};

config_shader_file!(
    /// The skybox vertex stage: a fullscreen triangle at the far plane.
    SKYBOX_VERTEX, "skybox/shaders/skybox_vertex.wgsl"
);
config_shader_file!(
    /// The fragment stage that samples an equirectangular panorama.
    EQUIRECT_FRAGMENT, "skybox/shaders/skybox_equirect_fragment.wgsl"
);
config_shader_file!(
    /// The fragment stage that samples a cubemap.
    CUBEMAP_FRAGMENT, "skybox/shaders/skybox_cubemap_fragment.wgsl"
);

/// Asset name of the skybox shader that samples an equirectangular panorama.
pub const EQUIRECT_SHADER_NAME: &str = "pill.skybox.shader.equirect";

/// The guid the equirect skybox shader is stored under, so a sky material file
/// can name it.
///
/// Derived from [`EQUIRECT_SHADER_NAME`] rather than drawn at random: the shader
/// is built in code, with no `.meta` file to keep a random guid in, and every
/// run must give it the same one.
pub const EQUIRECT_SHADER_GUID: AssetGuid = AssetGuid::from_name(EQUIRECT_SHADER_NAME);

/// Asset name of the skybox shader that samples a cubemap.
pub const CUBEMAP_SHADER_NAME: &str = "pill.skybox.shader.cubemap";

/// The guid the cubemap skybox shader is stored under; see
/// [`EQUIRECT_SHADER_GUID`].
pub const CUBEMAP_SHADER_GUID: AssetGuid = AssetGuid::from_name(CUBEMAP_SHADER_NAME);

/// The texture slot both skybox shaders read the sky from.
pub const SKY_SLOT: &str = "sky";

/// The shader assets [`install_shaders`] creates, and the files each is built
/// from.
pub const SHADER_SOURCES: &[ShaderSourceRecord] = &[
    ShaderSourceRecord {
        shader_asset_name: EQUIRECT_SHADER_NAME,
        vertex: SKYBOX_VERTEX,
        fragment: EQUIRECT_FRAGMENT,
    },
    ShaderSourceRecord {
        shader_asset_name: CUBEMAP_SHADER_NAME,
        vertex: SKYBOX_VERTEX,
        fragment: CUBEMAP_FRAGMENT,
    },
];

/// Install the equirect and the cubemap skybox shaders, and return them in
/// that order.
///
/// Idempotent: a store that already holds them gets the same handles back, so
/// this is safe to call once per generation - and it has to run before a sky
/// material file loads, because the file names its shader by guid.
///
/// # Errors
///
/// Returns an error when a name this owns is already taken by an asset of a
/// different type, or when a shader fails to build.
pub fn install_shaders(
    assets: &mut AssetManager,
) -> Result<(Handle<Shader>, Handle<Shader>), Box<dyn std::error::Error>> {
    let equirect = match assets.handle_by_name::<Shader>(EQUIRECT_SHADER_NAME) {
        Some(handle) => handle,
        None => assets.add_named_with_guid(
            EQUIRECT_SHADER_NAME,
            EQUIRECT_SHADER_GUID,
            skybox_shader(
                "pill_skybox_equirect",
                EQUIRECT_FRAGMENT,
                TextureType::Equirect,
            )?,
        )?,
    };
    let cubemap = match assets.handle_by_name::<Shader>(CUBEMAP_SHADER_NAME) {
        Some(handle) => handle,
        None => assets.add_named_with_guid(
            CUBEMAP_SHADER_NAME,
            CUBEMAP_SHADER_GUID,
            skybox_shader(
                "pill_skybox_cubemap",
                CUBEMAP_FRAGMENT,
                TextureType::Cubemap,
            )?,
        )?,
    };
    Ok((equirect, cubemap))
}

/// A sky material's parameters at their neutral values: no tint, unit
/// exposure, unrotated.
pub fn default_parameters() -> BTreeMap<String, MaterialParameter> {
    BTreeMap::from([
        (
            "skybox_tint".to_owned(),
            MaterialParameter::Color([1.0, 1.0, 1.0]),
        ),
        ("skybox_exposure".to_owned(), MaterialParameter::Scalar(1.0)),
        ("skybox_rotation".to_owned(), MaterialParameter::Scalar(0.0)),
    ])
}

/// A skybox pass named `name` that draws `material` into `target`.
///
/// Ordered with the geometry it sits behind (`order`); it reads the depth that
/// geometry wrote, so it must write the same target. A pass with no material
/// ([`Handle::INVALID`]) cannot be drawn, so it starts disabled.
pub fn skybox_pass(
    name: impl Into<String>,
    material: Handle<Material>,
    target: PassTarget,
    order: u8,
) -> RenderPass {
    RenderPass::new(name)
        .with_kind(PassKind::Skybox)
        .with_material(material)
        .with_target(target)
        .with_depth_write(false)
        .with_order(order)
        .with_enabled(material != Handle::INVALID)
}

/// One skybox shader: the far-plane vertex stage, `fragment`, the three sky
/// parameters, and a `sky` slot of `sky_type`.
fn skybox_shader(
    name: &str,
    fragment: ConfigShaderFile,
    sky_type: TextureType,
) -> AssetLoadResult<Shader> {
    Shader::new(name)
        .with_wgsl(SKYBOX_VERTEX.embedded_source, fragment.embedded_source)
        .with_parameter_slots([
            ShaderParameterSlot::new("skybox_tint", ShaderParameterType::Color),
            ShaderParameterSlot::new("skybox_exposure", ShaderParameterType::Scalar),
            ShaderParameterSlot::new("skybox_rotation", ShaderParameterType::Scalar),
        ])
        .with_texture_slots([ShaderTextureSlot::new(SKY_SLOT, sky_type, (0, 1))])
        .with_engine_parameters(true)
        .with_camera_parameters(true)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sky material files name the shaders by these digits, so they may never
    /// change; renaming a shader asset would.
    #[test]
    fn the_shader_guids_are_the_ones_material_files_name() {
        assert_eq!(
            EQUIRECT_SHADER_GUID.to_string(),
            "5628c9e4d9278fec79998a7adab65bc3"
        );
        assert_eq!(
            CUBEMAP_SHADER_GUID.to_string(),
            "a6dfb611b11dfb57c5039dd8b6fc1a46"
        );
    }

    #[test]
    fn installing_twice_returns_the_same_shaders() {
        let mut assets = AssetManager::new();

        let first = install_shaders(&mut assets).expect("free names");
        let second = install_shaders(&mut assets).expect("already installed");

        assert_eq!(first, second);
        assert_eq!(assets.guid_of(first.0), Some(EQUIRECT_SHADER_GUID));
        assert_eq!(assets.guid_of(first.1), Some(CUBEMAP_SHADER_GUID));
    }

    #[test]
    fn each_shader_declares_its_sky_slot_by_type() {
        let mut assets = AssetManager::new();
        let (equirect, cubemap) = install_shaders(&mut assets).expect("free names");

        let slot_type =
            |handle| assets.get::<Shader>(handle).unwrap().texture_slots[SKY_SLOT].texture_type;

        assert_eq!(slot_type(equirect), TextureType::Equirect);
        assert_eq!(slot_type(cubemap), TextureType::Cubemap);
    }

    #[test]
    fn a_pass_without_a_material_starts_disabled() {
        let pass = skybox_pass("sky", Handle::INVALID, PassTarget::Surface, 0);

        assert_eq!(pass.kind, PassKind::Skybox);
        assert!(!pass.enabled && !pass.depth_write);
    }
}
