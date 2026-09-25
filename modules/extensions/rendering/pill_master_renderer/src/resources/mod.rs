//! The renderer's own resources.
//!
//! # Responsibilities
//!
//! - One module per resource the renderer keeps: shaders, materials, meshes,
//!   textures, cameras, engine parameters, and the pipeline manager the game
//!   writes.
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
mod rendering_manager;

// --- Use ---

pub use renderer_shader::RendererShader;

pub use renderer_material::RendererMaterial;

pub use renderer_texture::RendererTexture;

pub use renderer_mesh::{RendererMesh, Vertex};

pub use renderer_camera::RendererCamera;

pub use engine_parameters::EngineParameters;

pub use renderer_resource_storage::RendererResourceStorage;

pub use renderer_pass::RendererPass;

pub use rendering_manager::RenderingManager;
