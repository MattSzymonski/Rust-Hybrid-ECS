//! Mesh assets and their interleaved vertex representation.

use pill_engine::Asset;

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct MeshVertex {
    pub position: [f32; 3],
    pub texture_coordinates: [f32; 2],
    pub normal: [f32; 3],
    pub tangent: [f32; 3],
    pub bitangent: [f32; 3],
}

#[derive(Clone, Debug)]
pub struct Mesh {
    pub name: String,
    pub vertices: Vec<MeshVertex>,
    pub indices: Vec<u32>,
}

impl Mesh {
    pub fn from_data(
        name: impl Into<String>,
        vertices: Vec<MeshVertex>,
        indices: Vec<u32>,
    ) -> Self {
        Self {
            name: name.into(),
            vertices,
            indices,
        }
    }

    pub fn triangle() -> Self {
        let vertex = |position, texture_coordinates| MeshVertex {
            position,
            texture_coordinates,
            normal: [0.0, 0.0, 1.0],
            tangent: [1.0, 0.0, 0.0],
            bitangent: [0.0, 1.0, 0.0],
        };
        Self::from_data(
            "triangle",
            vec![
                vertex([-0.5, -0.5, 0.0], [0.0, 1.0]),
                vertex([0.5, -0.5, 0.0], [1.0, 1.0]),
                vertex([0.0, 0.5, 0.0], [0.5, 0.0]),
            ],
            vec![0, 1, 2],
        )
    }

    /// Decodes a Wavefront OBJ buffer into an indexed, tangent-space mesh.
    ///
    /// The managed (C#) asset-loading bridge is the only other caller today:
    /// a managed project has no way to build a [`MeshVertex`] buffer itself,
    /// so it hands the host raw file bytes and gets a mesh back. This is the
    /// same decode `italian_brainrot`'s Rust project used to do for itself
    /// before that project moved to the shared bridge.
    pub fn from_obj_bytes(name: impl Into<String>, bytes: &[u8]) -> Result<Self, String> {
        let mut source = std::io::Cursor::new(bytes);
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
            return Err("the OBJ buffer contained no triangles".to_owned());
        }
        Ok(Self::from_data(name, vertices, indices))
    }
}

/// Accumulates and normalizes per-vertex tangent/bitangent vectors from the
/// mesh's triangles and their UV coordinates.
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

impl Asset for Mesh {}

#[cfg(test)]
mod tests {
    use super::*;

    const TRIANGLE_OBJ: &str = "\
v 0.0 0.0 0.0\n\
v 1.0 0.0 0.0\n\
v 0.0 1.0 0.0\n\
vt 0.0 0.0\n\
vt 1.0 0.0\n\
vt 0.0 1.0\n\
f 1/1 2/2 3/3\n";

    #[test]
    fn decodes_a_minimal_obj_into_an_indexed_triangle() {
        let mesh = Mesh::from_obj_bytes("triangle", TRIANGLE_OBJ.as_bytes()).expect("valid OBJ");
        assert_eq!(mesh.vertices.len(), 3);
        assert_eq!(mesh.indices, vec![0, 1, 2]);
    }

    #[test]
    fn rejects_a_buffer_with_no_triangles() {
        assert!(Mesh::from_obj_bytes("empty", b"").is_err());
    }
}
