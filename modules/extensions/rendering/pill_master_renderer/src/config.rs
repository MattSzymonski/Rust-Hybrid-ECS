//! Layout facts every pipeline the renderer builds shares.
//!
//! # Responsibilities
//!
//! - Name the bind group each kind of shader data occupies: the convention the
//!   engine's shaders declare and the drawers bind against.
//! - Size the instance batch the mesh drawer accumulates and uploads in one
//!   command.

/// Instances one draw command covers.
///
/// The drawer splits a frame's queue into chunks of this size and gives each
/// chunk its own region of the instance buffer, so it is a batching choice, not
/// a limit on how many instances a frame may draw.
pub const INSTANCE_BATCH_SIZE: usize = 10000;

/// Starting capacity of the drawer's staging instance vector.
pub const INITIAL_INSTANCE_VECTOR_CAPACITY: usize = 10000;

/// Bind group the engine's per-frame parameters occupy in every shader.
pub const ENGINE_PARAMETERS_BIND_GROUP_LAYOUT_INDEX: u32 = 0;
/// Bind group the active camera's parameters occupy in every shader.
pub const CAMERA_PARAMETERS_BIND_GROUP_LAYOUT_INDEX: u32 = 1;
/// Bind group a material's, or a pass's, uniform parameters occupy.
pub const MATERIAL_PARAMETERS_BIND_GROUP_LAYOUT_INDEX: u32 = 2;
/// Bind group a material's, or a pass's, textures occupy.
pub const MATERIAL_TEXTURES_BIND_GROUP_LAYOUT_INDEX: u32 = 3;
