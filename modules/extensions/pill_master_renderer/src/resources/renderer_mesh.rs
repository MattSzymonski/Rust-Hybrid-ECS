//! The GPU mesh resources the renderer draws with: the vertex and index
//! buffers behind every uploaded mesh.
//!
//! # Responsibilities
//!
//! - State the vertex buffer layout contract every vertex-bearing type
//!   implements, so a pipeline can describe its buffers without touching mesh
//!   data ([`Vertex`]).
//! - Upload a decoded [`Mesh`](crate::assets::Mesh) into the vertex and index
//!   buffers one draw needs, refusing a mesh that carries no geometry
//!   ([`RendererMesh`]).
//!
//! # Design
//!
//! The layout descriptor is derived from the interleaved
//! [`MeshVertex`](crate::assets::MeshVertex) fields rather than configured
//! per mesh, so the descriptor and the uploaded bytes cannot drift apart.
//! Every pipeline pairs it with the per-instance layout
//! ([`Instance`](crate::Instance)), and the two share one location space: the
//! mesh attributes take 0 and 4 through 7, the instance attributes take 1
//! through 3.

// External crates
use wgpu::util::DeviceExt;

// Current crate
use crate::{
    assets::{Mesh, MeshVertex},
    error::{RendererError, Result},
};

// --- Handle ---

pill_core::define_slot_key!(RendererMeshHandle);

// --- Vertex ---

/// A type whose values can sit in one vertex buffer: its memory layout, as
/// the render pipeline needs it to map the buffer into the shader.
///
/// [`RendererMesh`] describes the per-vertex buffer and
/// [`Instance`](crate::Instance) the per-instance buffer; every pipeline
/// takes both layouts, so a shader location belongs to exactly one of them.
pub trait Vertex {
    /// Returns the buffer layout the render pipeline maps this type through.
    ///
    /// The descriptor states the stride between vertices and where each
    /// attribute sits inside it, so it has to match both the bytes uploaded
    /// into the buffer and the shader locations the vertex shader declares.
    fn data_layout_descriptor<'a>() -> wgpu::VertexBufferLayout<'a>;
}

// --- Mesh ---

/// One mesh's GPU buffers: the vertex data and the index list that draws it.
///
/// Built during asset sync, when a mesh asset first arrives or its version
/// changes, and held in the resource storage beside every other GPU resource.
/// The draw path resolves a mesh handle per frame, so a rebuilt mesh is
/// picked up without invalidating any pipeline.
pub struct RendererMesh {
    /// Label used in logs and error messages, taken from the mesh asset.
    pub name: String,
    /// Vertex data, bound at slot 0 of every pipeline drawing this mesh.
    pub vertex_buffer: wgpu::Buffer,
    /// Index list into the vertex data, uploaded as `Uint32` indices.
    pub index_buffer: wgpu::Buffer,
    /// Number of indices a draw of this mesh passes, taken from the asset.
    pub index_count: u32,
}

impl RendererMesh {
    /// Uploads a mesh's vertices and indices, ready to draw.
    ///
    /// # Errors
    ///
    /// Returns [`RendererError::Other`] naming the mesh when it carries no
    /// vertices or no indices. A zero-sized buffer is not something wgpu will
    /// create, so the empty case has to be refused here - and a mesh with
    /// nothing in it is an asset mistake worth naming rather than an upload
    /// failure to dig out of the log.
    pub fn new(device: &wgpu::Device, name: &str, mesh_data: &Mesh) -> Result<Self> {
        if mesh_data.vertices.is_empty() || mesh_data.indices.is_empty() {
            return Err(RendererError::Other {
                detail: format!("mesh `{name}` has no vertices or no indices"),
            });
        }

        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some(&format!("{:?}_vertex_buffer", name)),
            contents: bytemuck::cast_slice(&mesh_data.vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some(&format!("{:?}_index_buffer", name)),
            contents: bytemuck::cast_slice(&mesh_data.indices),
            usage: wgpu::BufferUsages::INDEX,
        });

        let renderer_mesh = Self {
            name: name.to_string(),
            vertex_buffer,
            index_buffer,
            index_count: mesh_data.indices.len() as u32,
        };

        Ok(renderer_mesh)
    }
}

impl Vertex for RendererMesh {
    fn data_layout_descriptor<'a>() -> wgpu::VertexBufferLayout<'a> {
        use std::mem;
        wgpu::VertexBufferLayout {
            array_stride: mem::size_of::<MeshVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    // Vertex position
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x3,
                },
                wgpu::VertexAttribute {
                    // Vertex texture coordinates
                    // slangc maps TEXCOORD0 → @location(4), not 1
                    offset: mem::size_of::<[f32; 3]>() as wgpu::BufferAddress,
                    shader_location: 4,
                    format: wgpu::VertexFormat::Float32x2,
                },
                wgpu::VertexAttribute {
                    // Vertex normal
                    // slangc maps NORMAL → @location(5)
                    offset: mem::size_of::<[f32; 5]>() as wgpu::BufferAddress,
                    shader_location: 5,
                    format: wgpu::VertexFormat::Float32x3,
                },
                wgpu::VertexAttribute {
                    // Vertex tangent
                    // slangc maps TANGENT → @location(6)
                    offset: mem::size_of::<[f32; 8]>() as wgpu::BufferAddress,
                    shader_location: 6,
                    format: wgpu::VertexFormat::Float32x3,
                },
                wgpu::VertexAttribute {
                    // Vertex bitangent
                    // slangc maps BINORMAL → @location(7)
                    offset: mem::size_of::<[f32; 11]>() as wgpu::BufferAddress,
                    shader_location: 7,
                    format: wgpu::VertexFormat::Float32x3,
                },
            ],
        }
    }
}
