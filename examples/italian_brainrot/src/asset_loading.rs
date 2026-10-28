//! Decodes the bundled OBJ and PNG into the current renderer asset types.
//!
//! # Responsibilities
//!
//! - Ensure the bundled mesh, texture, shaders and materials exist in the
//!   world's asset manager, loading each only when it is absent.
//! - Hand the three ready materials back as handles, so the scene never
//!   looks an asset up by name.
//!
//! # Design
//!
//! A hot reload of the project calls `init` again on the same world, keeping
//! the asset manager, so add-or-reuse is what makes the reload idempotent;
//! a second `add_named` of the same name is an error by design.

use pill_engine::{AssetManager, Handle, World};
use pill_master_renderer_data::{
    AssetLoader, Material, Mesh, Shader, ShaderParameterSlot, ShaderParameterType,
    ShaderTextureKind, ShaderTextureSlot, Texture, TextureType,
};
use std::path::PathBuf;

/// The handles [`load`] returns: one mesh and one material per shading style.
#[derive(Clone, Copy)]
pub(crate) struct SceneAssets {
    pub mesh: Handle<Mesh>,
    pub lit: Handle<Material>,
    pub unlit: Handle<Material>,
    pub cartoon: Handle<Material>,
}

/// Ensures the bundled mesh, texture, shaders and materials exist in the
/// manager, building each one only when its name is absent.
///
/// # Errors
///
/// Returns an error when the manager is missing or when a bundled asset fails
/// to decode. An asset already present under its name is reused, so a project
/// reload - which calls `init` again on the same world - keeps its handles.
pub(crate) fn load(world: &mut World) -> Result<SceneAssets, Box<dyn std::error::Error>> {
    AssetLoader::set_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("res"));
    let assets = world
        .get_resource_mut::<AssetManager>()
        .ok_or_else(|| "the engine AssetManager resource is missing".to_owned())?;

    // The OBJ decode lives in the renderer; the loader supplies only the bytes.
    let mesh = match assets.handle_by_name::<Mesh>("italian_brainrot.mesh") {
        Some(handle) => handle,
        None => {
            let obj = AssetLoader::Path("models/chimpanzini_bananini.obj".into()).load()?;
            assets.add_named(
                "italian_brainrot.mesh",
                // The bundled file is the original demo's: its UVs are
                // authored top-down, against a sampler whose V starts at the
                // image's top edge, so the default bottom-up flip would
                // scatter every island over the texture.
                Mesh::from_obj_bytes_top_down("chimpanzini_bananini", &obj)?,
            )?
        }
    };
    let color = match assets.handle_by_name::<Texture>("italian_brainrot.color") {
        Some(handle) => handle,
        None => assets.add_named(
            "italian_brainrot.color",
            Texture::new(
                "chimpanzini_bananini",
                TextureType::Color,
                AssetLoader::Path("textures/chimpanzini_bananini_color.png".into()),
            )?,
        )?,
    };
    let unlit_shader = match assets.handle_by_name::<Shader>("italian_brainrot.shader.unlit") {
        Some(handle) => handle,
        None => assets.add_named(
            "italian_brainrot.shader.unlit",
            Shader::new("italian_brainrot_unlit")
                .with_vertex_source(AssetLoader::Path("shaders/default_vertex.wgsl".into()))
                .with_fragment_source(AssetLoader::Path("shaders/unlit_fragment.wgsl".into()))
                .with_parameter_slots(vec![ShaderParameterSlot::new(
                    "tint",
                    ShaderParameterType::Color,
                )])
                .with_texture_slots(vec![ShaderTextureSlot::new(
                    "color",
                    ShaderTextureKind::Color,
                    (0, 1),
                )])
                .with_engine_parameters(true)
                .with_camera_parameters(true)
                .build()?,
        )?,
    };
    let cartoon_shader = match assets.handle_by_name::<Shader>("italian_brainrot.shader.cartoon") {
        Some(handle) => handle,
        None => assets.add_named(
            "italian_brainrot.shader.cartoon",
            Shader::new("cartoon")
                .with_vertex_source(AssetLoader::Path("shaders/default_vertex.wgsl".into()))
                .with_fragment_source(AssetLoader::Path("shaders/cartoon_fragment.wgsl".into()))
                .with_parameter_slots(vec![ShaderParameterSlot::new(
                    "posterize_level",
                    ShaderParameterType::Scalar,
                )])
                .with_texture_slots(vec![ShaderTextureSlot::new(
                    "color",
                    ShaderTextureKind::Color,
                    (0, 1),
                )])
                .with_engine_parameters(true)
                .with_camera_parameters(true)
                .build()?,
        )?,
    };

    // An invalid shader handle selects the renderer's built-in lit shader.
    let lit = match assets.handle_by_name::<Material>("italian_brainrot.material.lit") {
        Some(handle) => handle,
        None => assets.add_named(
            "italian_brainrot.material.lit",
            Material::builder("chimpanzini_bananini_lit")
                .texture("color", &color)
                .color_parameter("tint", [1.0; 3])
                .scalar_parameter("specularity", 0.5)
                .build(),
        )?,
    };
    let unlit = match assets.handle_by_name::<Material>("italian_brainrot.material.unlit") {
        Some(handle) => handle,
        None => assets.add_named(
            "italian_brainrot.material.unlit",
            Material::builder("chimpanzini_bananini_unlit")
                .shader(&unlit_shader)
                .texture("color", &color)
                .color_parameter("tint", [1.0; 3])
                .build(),
        )?,
    };
    let cartoon = match assets.handle_by_name::<Material>("italian_brainrot.material.cartoon") {
        Some(handle) => handle,
        None => assets.add_named(
            "italian_brainrot.material.cartoon",
            Material::builder("chimpanzini_bananini_cartoon")
                .shader(&cartoon_shader)
                .texture("color", &color)
                .scalar_parameter("posterize_level", 3.0)
                .build(),
        )?,
    };

    Ok(SceneAssets {
        mesh,
        lit,
        unlit,
        cartoon,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_model_decodes_to_indexed_triangles() {
        AssetLoader::set_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("res"));
        let obj = AssetLoader::Path("models/chimpanzini_bananini.obj".into())
            .load()
            .expect("the bundled OBJ is readable");
        let mesh =
            Mesh::from_obj_bytes_top_down("chimpanzini_bananini", &obj).expect("a valid OBJ");

        assert!(!mesh.vertices.is_empty());
        assert!(!mesh.indices.is_empty());
        assert_eq!(mesh.indices.len() % 3, 0);
    }

    #[test]
    fn bundled_texture_decodes_to_rgba() {
        AssetLoader::set_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("res"));
        let texture = Texture::new(
            "chimpanzini_bananini",
            TextureType::Color,
            AssetLoader::Path("textures/chimpanzini_bananini_color.png".into()),
        )
        .expect("bundled PNG");
        assert!(texture.width > 0 && texture.height > 0);
        assert_eq!(
            texture.rgba.len(),
            texture.width as usize * texture.height as usize * 4
        );
    }
}
