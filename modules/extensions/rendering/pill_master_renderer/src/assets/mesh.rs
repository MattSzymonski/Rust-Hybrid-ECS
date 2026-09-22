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
}

impl Asset for Mesh {}
