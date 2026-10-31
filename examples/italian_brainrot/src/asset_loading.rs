//! Loads the bundled model and texture by path and builds the materials.
//!
//! # Responsibilities
//!
//! - Mount the project's `res` and ask the world for the model and the colour
//!   texture by path, so their guids come from the `.meta` files beside them.
//! - Build the shaders and the three materials in code, since a shader is a
//!   pair of source files rather than one imported asset.
//! - Hand the handles back, so the scene never looks an asset up by name.
//!
//! # Design
//!
//! A hot reload of the project calls `init` again on the same world, keeping
//! the asset manager; `World::load_asset` hands back the existing handle for a
//! path that is already loaded, which is what makes the reload idempotent.

use pill_engine::{AssetManager, Handle, World};
use pill_master_renderer_data::{
    AssetLoader, Material, Mesh, Shader, ShaderParameterSlot, ShaderParameterType,
    ShaderTextureKind, ShaderTextureSlot, Texture,
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

/// Mounts the bundled `res` and loads the model and texture by path, then
/// builds the shaders and materials around those handles.
///
/// # Errors
///
/// Returns an error when a bundled asset fails to load or the manager is
/// missing. Both loads return the handle a previous run already holds, so a
/// project reload keeps its assets.
pub(crate) fn load(world: &mut World) -> Result<SceneAssets, Box<dyn std::error::Error>> {
    /// The bundled model and colour texture, relative to `res`.
    const MODEL_PATH: &str = "models/chimpanzini_bananini.obj";
    const TEXTURE_PATH: &str = "textures/chimpanzini_bananini_color.png";

    // Mounting the directory is what the path-based load resolves through; the
    // shader builders below read their sources through the same mount.
    AssetLoader::set_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("res"));

    // The mesh's `.meta` states `flip_v: false`: the bundled file is the
    // original demo's, whose UVs are authored top-down against a sampler whose
    // V starts at the image's top edge.
    let mesh = world.load_asset::<Mesh>(MODEL_PATH)?;
    let color = world.load_asset::<Texture>(TEXTURE_PATH)?;

    let assets = world
        .get_resource_mut::<AssetManager>()
        .ok_or_else(|| "the engine AssetManager resource is missing".to_owned())?;
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
    use pill_engine::ImportedAsset;
    use pill_master_renderer_data::{MeshImportSettings, TextureImportSettings};

    /// Reads a bundled file through the same mount the game uses.
    fn bundled(path: &str) -> Vec<u8> {
        AssetLoader::set_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("res"));
        AssetLoader::Path(path.into())
            .load()
            .expect("the bundled file is readable")
    }

    #[test]
    fn the_mesh_metadata_keeps_the_bundled_uvs_top_down() {
        let metadata = bundled("models/chimpanzini_bananini.obj.meta");
        let text = String::from_utf8(metadata).expect("the sidecar is UTF-8");
        assert!(
            text.contains("\"flip_v\": false"),
            "the model is authored top-down, so its sidecar must not flip V: {text}"
        );
    }

    #[test]
    fn the_bundled_model_imports_to_indexed_triangles() {
        let obj = bundled("models/chimpanzini_bananini.obj");
        let settings = MeshImportSettings {
            flip_v: false,
            calculate_tangents: true,
        };
        let mesh = Mesh::import("chimpanzini_bananini", &obj, &settings).expect("a valid OBJ");

        assert!(!mesh.vertices.is_empty());
        assert!(!mesh.indices.is_empty());
        assert_eq!(mesh.indices.len() % 3, 0);
    }

    #[test]
    fn the_bundled_texture_imports_as_a_colour_image() {
        let png = bundled("textures/chimpanzini_bananini_color.png");
        let texture = Texture::import(
            "chimpanzini_bananini",
            &png,
            &TextureImportSettings::default(),
        )
        .expect("a valid PNG");
        assert!(texture.width > 0 && texture.height > 0);
        assert_eq!(
            texture.rgba.len(),
            texture.width as usize * texture.height as usize * 4
        );
    }
}
