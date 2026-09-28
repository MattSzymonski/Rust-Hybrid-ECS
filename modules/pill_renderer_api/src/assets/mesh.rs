//! Mesh assets and their interleaved vertex representation.
//!
//! # Responsibilities
//!
//! - Carry the geometry the renderer uploads: vertices interleaved in the
//!   order the pipeline's vertex step reads them, plus a 32-bit index list.
//! - Build meshes three ways: from buffers the game already holds, from the
//!   built-in triangle used as a stand-in, and by decoding a Wavefront OBJ
//!   buffer handed over by the managed asset bridge.
//! - Derive tangent space after an OBJ decode. A file carries positions, UVs
//!   and normals, so the tangents and bitangents the shaders read are
//!   accumulated from the mesh's triangles and normalized here.
//!
//! # Design
//!
//! [`MeshVertex`] is `repr(C)` and `Pod`, so a mesh's vertices upload as a
//! plain byte slice, and that same field order is the attribute layout the
//! renderer declares to wgpu. A mesh is an [`Asset`], so the renderer
//! re-uploads its buffers when the asset's version moves rather than
//! expecting a game to mutate renderer state mid frame.

// External crates
use pill_engine::{Asset, AssetLoadError};

/// One vertex, in the layout the vertex buffer step reads it.
///
/// `repr(C)` and `Pod` are what let a run of these upload to the GPU as raw
/// bytes, and the field order is the attribute layout the renderer declares
/// to wgpu, so the two have to agree.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct MeshVertex {
    /// Object-space position.
    pub position: [f32; 3],
    /// Texture coordinates for the shader's UV input. The OBJ decoder flips
    /// V, because a file counts it upward from the bottom edge.
    pub texture_coordinates: [f32; 2],
    /// Object-space normal. A decoded vertex the file gives no normal for
    /// falls back to +Y.
    pub normal: [f32; 3],
    /// Tangent along the U direction, one axis of the TBN frame the shaders
    /// use for normal mapping.
    pub tangent: [f32; 3],
    /// Bitangent along the V direction, the frame's third axis. The OBJ
    /// decode starts both at zero and fills them from the triangles.
    pub bitangent: [f32; 3],
}

/// One drawable piece of geometry: interleaved vertices plus a 32-bit index
/// list.
///
/// The renderer builds a GPU copy once and rebuilds it when the asset's
/// version moves, so a game edits the mesh it holds and lets the renderer
/// notice rather than touching buffers itself.
#[derive(Clone, Debug)]
pub struct Mesh {
    /// Label used in logs, profiling, and error messages.
    pub name: String,
    /// Vertices, interleaved as [`MeshVertex`] lays them out.
    pub vertices: Vec<MeshVertex>,
    /// Indices into [`Self::vertices`], one triangle per three entries.
    pub indices: Vec<u32>,
}

impl Mesh {
    /// Builds a mesh from finished vertex and index buffers.
    ///
    /// The buffers are stored as given: nothing checks the indices against
    /// the vertex list, so the caller supplies a pair that already agrees.
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

    /// The built-in stand-in mesh: one triangle in the XY plane, half a unit
    /// quad, facing +Z with its UVs and tangent space filled in.
    ///
    /// A project with no geometry of its own can register this and draw it
    /// through the same material and pipeline path as any loaded mesh, which
    /// keeps a fresh scene renderable before its assets exist.
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
    ///
    /// # Errors
    ///
    /// Returns [`AssetLoadError::Decode`] when the buffer is not a readable
    /// OBJ, or when it holds no triangles - the two ways a mesh the renderer
    /// would otherwise upload empty comes back named instead. Both carry the
    /// mesh's name, so the failure points at the asset rather than at the
    /// bytes.
    pub fn from_obj_bytes(name: impl Into<String>, bytes: &[u8]) -> Result<Self, AssetLoadError> {
        let name = name.into();
        let mut source = std::io::Cursor::new(bytes);
        let options = tobj::LoadOptions {
            triangulate: true,
            single_index: true,
            ..Default::default()
        };
        let (models, _) = tobj::load_obj_buf(&mut source, &options, |_| {
            Ok((Vec::new(), Default::default()))
        })
        .map_err(|error| AssetLoadError::Decode {
            label: name.clone(),
            detail: error.to_string(),
        })?;

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
            return Err(AssetLoadError::Decode {
                label: name,
                detail: "the OBJ buffer contained no triangles".to_owned(),
            });
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

/// Returns the normalized vector, or the fallback axis when the vector is
/// non-finite or too short to normalize.
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
