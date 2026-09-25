//! Decodes the bundled OBJ and PNG into the current renderer asset types.

use pill_engine::{AssetManager, Handle, World};
use pill_master_renderer::{
    AssetLoader, Material, Mesh, Shader, ShaderParameterSlot, ShaderParameterType,
    ShaderTextureSlot, Texture, TextureType,
};
use std::{collections::HashMap, path::PathBuf};

#[derive(Clone, Copy)]
pub(crate) struct SceneAssets {
    pub mesh: Handle<Mesh>,
    pub lit: Handle<Material>,
    pub unlit: Handle<Material>,
    pub cartoon: Handle<Material>,
}

/// Decodes the bundled mesh, texture, shaders and materials into the manager.
///
/// # Errors
///
/// Returns an error when the manager is missing, when a bundled asset fails to
/// decode, or when one of the names is already taken - a second load of the
/// same six names is a mistake worth reporting rather than a silent duplicate.
pub(crate) fn load(world: &mut World) -> Result<SceneAssets, Box<dyn std::error::Error>> {
    AssetLoader::set_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("res"));
    let assets = world
        .get_resource_mut::<AssetManager>()
        .ok_or_else(|| "the engine AssetManager resource is missing".to_owned())?;

    // The OBJ decode lives in the renderer; the loader supplies only the bytes.
    let obj = AssetLoader::Path("models/chimpanzini_bananini.obj".into()).load()?;
    let mesh = assets.add_named(
        "italian_brainrot.mesh",
        Mesh::from_obj_bytes("chimpanzini_bananini", &obj)?,
    )?;
    let color = assets.add_named(
        "italian_brainrot.color",
        Texture::new(
            "chimpanzini_bananini",
            TextureType::Color,
            AssetLoader::Path("textures/chimpanzini_bananini_color.png".into()),
        )?,
    )?;
    let unlit_shader = assets.add_named(
        "italian_brainrot.shader.unlit",
        Shader::new(
            "italian_brainrot_unlit",
            AssetLoader::Path("shaders/default_vertex.wgsl".into()),
            AssetLoader::Path("shaders/unlit_fragment.wgsl".into()),
            vec![(
                "tint".to_owned(),
                ShaderParameterSlot::new(ShaderParameterType::Color),
            )],
            HashMap::from([(
                "color".to_owned(),
                ShaderTextureSlot::new(TextureType::Color, (0, 1)),
            )]),
            true,
            true,
        )?,
    )?;
    let cartoon_shader = assets.add_named(
        "italian_brainrot.shader.cartoon",
        Shader::new(
            "cartoon",
            AssetLoader::Path("shaders/default_vertex.wgsl".into()),
            AssetLoader::Path("shaders/cartoon_fragment.wgsl".into()),
            vec![(
                "posterize_level".to_owned(),
                ShaderParameterSlot::new(ShaderParameterType::Scalar),
            )],
            HashMap::from([(
                "color".to_owned(),
                ShaderTextureSlot::new(TextureType::Color, (0, 1)),
            )]),
            true,
            true,
        )?,
    )?;

    // An invalid shader handle selects the renderer's built-in lit shader.
    let lit = assets.add_named(
        "italian_brainrot.material.lit",
        Material::builder("chimpanzini_bananini_lit")
            .texture("color", &color)
            .color_parameter("tint", [1.0; 3])
            .scalar_parameter("specularity", 0.5)
            .build(),
    )?;
    let unlit = assets.add_named(
        "italian_brainrot.material.unlit",
        Material::builder("chimpanzini_bananini_unlit")
            .shader(&unlit_shader)
            .texture("color", &color)
            .color_parameter("tint", [1.0; 3])
            .build(),
    )?;
    let cartoon = assets.add_named(
        "italian_brainrot.material.cartoon",
        Material::builder("chimpanzini_bananini_cartoon")
            .shader(&cartoon_shader)
            .texture("color", &color)
            .scalar_parameter("posterize_level", 3.0)
            .build(),
    )?;

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
        let mesh = Mesh::from_obj_bytes("chimpanzini_bananini", &obj).expect("a valid OBJ");

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
