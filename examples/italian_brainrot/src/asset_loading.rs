//! Decodes the bundled OBJ and PNG into the current renderer asset types.

use pill_engine::{AssetManager, Handle, World};
use pill_master_renderer::{
    AssetLoader, Material, Mesh, MeshVertex, Shader, ShaderParameterSlot, ShaderParameterType,
    ShaderTextureSlot, Texture, TextureType,
};
use std::{collections::HashMap, io::Cursor, path::PathBuf};

const MODEL_BYTES: &[u8] = include_bytes!("../res/models/chimpanzini_bananini.obj");

#[derive(Clone, Copy)]
pub(crate) struct SceneAssets {
    pub mesh: Handle<Mesh>,
    pub lit: Handle<Material>,
    pub unlit: Handle<Material>,
    pub cartoon: Handle<Material>,
}

pub(crate) fn load(world: &mut World) -> Result<SceneAssets, String> {
    AssetLoader::set_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("res"));
    let assets = world
        .get_resource_mut::<AssetManager>()
        .ok_or_else(|| "the engine AssetManager resource is missing".to_owned())?;

    let mesh = named_or_insert_with(assets, "italian_brainrot.mesh", || decode_mesh(MODEL_BYTES))?;
    let color = named_or_insert_with(assets, "italian_brainrot.color", || {
        Texture::new(
            "chimpanzini_bananini",
            TextureType::Color,
            AssetLoader::Path("textures/chimpanzini_bananini_color.png".into()),
        )
        .map_err(|error| error.to_string())
    })?;
    let unlit_shader = named_or_insert_with(assets, "italian_brainrot.shader.unlit", || {
        Shader::new(
            "italian_brainrot_unlit",
            AssetLoader::Path("shaders/default_vertex.wgsl".into()),
            AssetLoader::Path("shaders/unlit_fragment.wgsl".into()),
            vec![(
                "tint".to_owned(),
                ShaderParameterSlot::new(ShaderParameterType::Color),
            )],
            color_texture_slots(),
            true,
            true,
        )
        .map_err(|error| error.to_string())
    })?;
    let cartoon_shader = named_or_insert_with(assets, "italian_brainrot.shader.cartoon", || {
        Shader::new(
            "cartoon",
            AssetLoader::Path("shaders/default_vertex.wgsl".into()),
            AssetLoader::Path("shaders/cartoon_fragment.wgsl".into()),
            vec![(
                "posterize_level".to_owned(),
                ShaderParameterSlot::new(ShaderParameterType::Scalar),
            )],
            color_texture_slots(),
            true,
            true,
        )
        .map_err(|error| error.to_string())
    })?;

    // An invalid shader handle selects the renderer's built-in lit shader.
    let lit = named_or_insert_with(assets, "italian_brainrot.material.lit", || {
        Ok(Material::builder("chimpanzini_bananini_lit")
            .texture("color", &color)
            .color_parameter("tint", [1.0; 3])
            .scalar_parameter("specularity", 0.5)
            .build())
    })?;
    let unlit = named_or_insert_with(assets, "italian_brainrot.material.unlit", || {
        Ok(Material::builder("chimpanzini_bananini_unlit")
            .shader(&unlit_shader)
            .texture("color", &color)
            .color_parameter("tint", [1.0; 3])
            .build())
    })?;
    let cartoon = named_or_insert_with(assets, "italian_brainrot.material.cartoon", || {
        Ok(Material::builder("chimpanzini_bananini_cartoon")
            .shader(&cartoon_shader)
            .texture("color", &color)
            .scalar_parameter("posterize_level", 3.0)
            .build())
    })?;

    Ok(SceneAssets {
        mesh,
        lit,
        unlit,
        cartoon,
    })
}

fn named_or_insert_with<T, F>(
    assets: &mut AssetManager,
    name: &str,
    create: F,
) -> Result<Handle<T>, String>
where
    T: pill_engine::Asset + trait_type_map::TraitAccessible<dyn pill_engine::Asset>,
    F: FnOnce() -> Result<T, String>,
{
    if let Some(handle) = assets.handle_by_name::<T>(name) {
        return Ok(handle);
    }
    Ok(assets.add_named(name, create()?))
}

fn color_texture_slots() -> HashMap<String, ShaderTextureSlot> {
    HashMap::from([(
        "color".to_owned(),
        ShaderTextureSlot::new(TextureType::Color, (0, 1)),
    )])
}

fn decode_mesh(bytes: &[u8]) -> Result<Mesh, String> {
    let mut source = Cursor::new(bytes);
    let options = tobj::LoadOptions {
        triangulate: true,
        single_index: true,
        ..Default::default()
    };
    let (models, _) = tobj::load_obj_buf(&mut source, &options, |_| {
        Ok((Vec::new(), Default::default()))
    })
    .map_err(|error| error.to_string())?;

    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for model in models {
        let source = model.mesh;
        let base = vertices.len() as u32;
        for index in 0..source.positions.len() / 3 {
            let position = [
                source.positions[index * 3],
                source.positions[index * 3 + 1],
                source.positions[index * 3 + 2],
            ];
            let texture_coordinates = if source.texcoords.len() >= index * 2 + 2 {
                [
                    source.texcoords[index * 2],
                    1.0 - source.texcoords[index * 2 + 1],
                ]
            } else {
                [0.0; 2]
            };
            let normal = if source.normals.len() >= index * 3 + 3 {
                [
                    source.normals[index * 3],
                    source.normals[index * 3 + 1],
                    source.normals[index * 3 + 2],
                ]
            } else {
                [0.0, 1.0, 0.0]
            };
            vertices.push(MeshVertex {
                position,
                texture_coordinates,
                normal,
                tangent: [0.0; 3],
                bitangent: [0.0; 3],
            });
        }
        indices.extend(source.indices.into_iter().map(|index| base + index));
    }

    calculate_tangent_space(&mut vertices, &indices);
    if vertices.is_empty() || indices.is_empty() {
        return Err("the bundled OBJ contained no triangles".to_owned());
    }
    Ok(Mesh::from_data("chimpanzini_bananini", vertices, indices))
}

fn calculate_tangent_space(vertices: &mut [MeshVertex], indices: &[u32]) {
    for triangle in indices.chunks_exact(3) {
        let [a, b, c] = [
            triangle[0] as usize,
            triangle[1] as usize,
            triangle[2] as usize,
        ];
        let p0 = glam::Vec3::from(vertices[a].position);
        let p1 = glam::Vec3::from(vertices[b].position);
        let p2 = glam::Vec3::from(vertices[c].position);
        let uv0 = glam::Vec2::from(vertices[a].texture_coordinates);
        let uv1 = glam::Vec2::from(vertices[b].texture_coordinates);
        let uv2 = glam::Vec2::from(vertices[c].texture_coordinates);
        let edge1 = p1 - p0;
        let edge2 = p2 - p0;
        let delta1 = uv1 - uv0;
        let delta2 = uv2 - uv0;
        let determinant = delta1.x * delta2.y - delta1.y * delta2.x;
        if determinant.abs() < 1.0e-8 {
            continue;
        }
        let reciprocal = determinant.recip();
        let tangent = (edge1 * delta2.y - edge2 * delta1.y) * reciprocal;
        let bitangent = (edge2 * delta1.x - edge1 * delta2.x) * reciprocal;
        for index in [a, b, c] {
            vertices[index].tangent = (glam::Vec3::from(vertices[index].tangent) + tangent).into();
            vertices[index].bitangent =
                (glam::Vec3::from(vertices[index].bitangent) + bitangent).into();
        }
    }
    for vertex in vertices {
        vertex.tangent = normalized_or(glam::Vec3::from(vertex.tangent), glam::Vec3::X).into();
        vertex.bitangent = normalized_or(glam::Vec3::from(vertex.bitangent), glam::Vec3::Y).into();
    }
}

fn normalized_or(value: glam::Vec3, fallback: glam::Vec3) -> glam::Vec3 {
    if value.is_finite() && value.length_squared() > 1.0e-8 {
        value.normalize()
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_model_decodes_to_indexed_triangles() {
        let mesh = decode_mesh(MODEL_BYTES).expect("bundled OBJ");
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
