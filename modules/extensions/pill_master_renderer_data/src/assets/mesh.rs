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
//! - Load through a metadata file ([`ImportedAsset`]): the
//!   [`MeshImportSettings`] in `<model>.obj.meta` hold the decode choices.
//!
//! # Design
//!
//! [`MeshVertex`] is `repr(C)` and `Pod`, so a mesh's vertices upload as a
//! plain byte slice, and that same field order is the attribute layout the
//! renderer declares to wgpu. A mesh is an [`Asset`], so the renderer
//! re-uploads its buffers when the asset's version moves rather than
//! expecting a game to mutate renderer state mid frame.

// External crates
use pill_engine::{Asset, AssetLoadError, AssetLoadResult, ImportedAsset};
use serde::{Deserialize, Serialize};

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
        Self::decode_obj(name.into(), bytes, &MeshImportSettings::default())
    }

    /// The OBJ decode behind [`Self::from_obj_bytes`] and the metadata import,
    /// with the choices `settings` controls.
    fn decode_obj(
        name: String,
        bytes: &[u8],
        settings: &MeshImportSettings,
    ) -> Result<Self, AssetLoadError> {
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
                    // A file counts V upward from the bottom edge; the
                    // renderer samples from the top.
                    let v_coordinate = source.texcoords[index * 2 + 1];
                    [
                        source.texcoords[index * 2],
                        if settings.flip_v {
                            1.0 - v_coordinate
                        } else {
                            v_coordinate
                        },
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

        if settings.calculate_tangents {
            calculate_tangent_space(&mut vertices, &indices);
        }
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

/// The pinned shared name of [`Mesh`]; see the comment on its `Asset` impl.
const MESH_SHARED_NAME: &str = "pill_master_renderer::assets::Mesh";

// Shared across binaries: the data module, the GPU module and every project
// compile their own copy of this crate, each with its own `TypeId`. The pinned
// name makes them one asset column (see `Asset::shared_name`); keep it
// verbatim when moving the type.
impl Asset for Mesh {
    fn shared_name() -> Option<&'static str> {
        Some(MESH_SHARED_NAME)
    }

    fn shared_identity() -> Option<u128> {
        // A `const`, so the name is hashed at compile time. The default hashes
        // it on every call, and every `AssetManager` lookup makes that call:
        // the renderer does it several times per drawn entity, every frame.
        const IDENTITY: u128 = pill_engine::component::shared_component_identity(MESH_SHARED_NAME);
        Some(IDENTITY)
    }
}

/// How to decode a model file into a mesh: what its `.meta` file holds.
///
/// The defaults are the choices [`Mesh::from_obj_bytes`] always made, so a
/// mesh imported without a metadata file decodes exactly as before. Every
/// field is part of the metadata file format; `#[serde(default)]` lets a file
/// written before a field existed still load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MeshImportSettings {
    /// Flip the V texture coordinate (`v` becomes `1 - v`). A file counts V
    /// upward from the bottom edge; the renderer samples from the top.
    pub flip_v: bool,
    /// Derive tangents and bitangents from the triangles and their UVs, which
    /// normal mapping needs. Off leaves them zero.
    pub calculate_tangents: bool,
}

impl Default for MeshImportSettings {
    fn default() -> Self {
        Self {
            flip_v: true,
            calculate_tangents: true,
        }
    }
}

// The metadata type name defaults to the shared name above, so every binary's
// copy of `Mesh` reads the same files.
impl ImportedAsset for Mesh {
    type ImportSettings = MeshImportSettings;
    const SOURCE_EXTENSIONS: &'static [&'static str] = &["obj"];

    fn import(
        name: &str,
        source_bytes: &[u8],
        settings: &MeshImportSettings,
    ) -> AssetLoadResult<Self> {
        Self::decode_obj(name.to_owned(), source_bytes, settings)
    }
}

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

    /// The default settings are the old fixed behaviour, so an import with no
    /// metadata file gives the same mesh `from_obj_bytes` always did.
    #[test]
    fn importing_with_default_settings_matches_from_obj_bytes() {
        let decoded = Mesh::from_obj_bytes("t", TRIANGLE_OBJ.as_bytes()).unwrap();
        let imported =
            Mesh::import("t", TRIANGLE_OBJ.as_bytes(), &MeshImportSettings::default()).unwrap();
        let positions_and_uvs = |mesh: &Mesh| {
            mesh.vertices
                .iter()
                .map(|vertex| (vertex.position, vertex.texture_coordinates, vertex.tangent))
                .collect::<Vec<_>>()
        };
        assert_eq!(positions_and_uvs(&imported), positions_and_uvs(&decoded));
        // V was flipped: the second texture coordinate of `vt 0.0 1.0` is 0.
        assert_eq!(imported.vertices[2].texture_coordinates, [0.0, 0.0]);
    }

    #[test]
    fn settings_control_the_v_flip_and_the_tangents() {
        let settings = MeshImportSettings {
            flip_v: false,
            calculate_tangents: false,
        };
        let mesh = Mesh::import("t", TRIANGLE_OBJ.as_bytes(), &settings).unwrap();
        assert_eq!(mesh.vertices[2].texture_coordinates, [0.0, 1.0]);
        assert!(mesh
            .vertices
            .iter()
            .all(|vertex| vertex.tangent == [0.0; 3]));

        let with_tangents =
            Mesh::import("t", TRIANGLE_OBJ.as_bytes(), &MeshImportSettings::default()).unwrap();
        assert!(with_tangents
            .vertices
            .iter()
            .any(|vertex| vertex.tangent != [0.0; 3]));
    }

    /// The on-disk form: field names as written, and defaults for missing ones.
    #[test]
    fn settings_serialize_with_their_field_names() {
        assert_eq!(
            serde_json::to_string(&MeshImportSettings::default()).unwrap(),
            r#"{"flip_v":true,"calculate_tangents":true}"#
        );
        let partial: MeshImportSettings = serde_json::from_str(r#"{"flip_v": false}"#).unwrap();
        assert_eq!(
            partial,
            MeshImportSettings {
                flip_v: false,
                calculate_tangents: true
            }
        );
    }
}
