//! Placement of an entity in the scene.
//!
//! # Responsibilities
//!
//! - Define [`TransformComponent`]: the position, orientation and scale the
//!   renderer reads for every drawable and for the camera it renders through.
//!
//! # Design
//!
//! Shared and persistable, so a write here is visible to the managed side and
//! survives a project reload. The shared name is pinned rather than derived
//! from the module path, for the reason [`crate::components`] gives.

// External crates
use pill_engine::{PillComponent, World};
use serde::{Deserialize, Serialize};

/// Placement of an entity in the scene: position, orientation and scale.
///
/// The renderer reads it for every drawable and for the camera it renders
/// through. Because the component is shared and persistable, one write here is
/// visible to the managed side and survives a project reload.
#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PillComponent)]
// Pinned to the name this type carried before it moved into `components/`. A
// derived name follows `module_path!()`, so re-deriving it would register a
// different component and orphan every column a live world holds for this one.
#[pill(
    shared = "pill_master_renderer::component::TransformComponent",
    persistable
)]
pub struct TransformComponent {
    /// Position of the entity's origin, in world units.
    pub translation: [f32; 3],
    /// Orientation as a unit quaternion `[x, y, z, w]`; identity is
    /// `[0, 0, 0, 1]`. A non-finite or zero-length value is read as identity
    /// instead of being used as-is.
    pub rotation: [f32; 4],
    /// Per-axis scale applied to the mesh when it is drawn.
    pub scale: [f32; 3],
}

impl Default for TransformComponent {
    fn default() -> Self {
        Self {
            translation: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
        }
    }
}

/// Registers [`TransformComponent`] with the world, field layout included.
///
/// The derive generates `__pill_register_TransformComponent` as a private item
/// of this module, so [`crate::components::register_components`] reaches it
/// through here rather than by path.
pub(crate) fn register(world: &mut World) {
    __pill_register_TransformComponent(world);
}
