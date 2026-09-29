//! The renderer's own resources.
//!
//! # Responsibilities
//!
//! - One module per resource the renderer keeps: shaders, materials, meshes,
//!   textures, cameras, engine parameters, and the pipeline manager the game
//!   writes.
//! - Define each resource's handle beside the resource it names, so a type and
//!   the key its slot map accepts cannot drift apart.
//! - Re-export them flat, so the renderer and the drawers reach a resource by
//!   name rather than by path.

mod engine_parameters;
mod renderer_camera;
mod renderer_material;
mod renderer_mesh;
mod renderer_pass;
mod renderer_resource_storage;
mod renderer_shader;
mod renderer_texture;

// --- Use ---

pub use renderer_shader::RendererShader;

pub use renderer_material::RendererMaterial;

pub use renderer_texture::RendererTexture;

pub use renderer_mesh::{RendererMesh, Vertex};

pub use renderer_camera::RendererCamera;

// The handles name slots in the renderer's own maps, so they are plumbing
// rather than interface: reachable anywhere in the crate, exported nowhere.
// They were private when they lived in one `resource_handles` module, and
// defining them here does not make them part of the crate's surface.
pub(crate) use renderer_camera::RendererCameraHandle;
pub(crate) use renderer_material::RendererMaterialHandle;
pub(crate) use renderer_mesh::RendererMeshHandle;
pub(crate) use renderer_shader::RendererShaderHandle;
pub(crate) use renderer_texture::RendererTextureHandle;

pub use engine_parameters::EngineParameters;

pub use renderer_resource_storage::RendererResourceStorage;

pub use renderer_pass::RendererPass;

pub use pill_renderer_api::RenderingManager;
