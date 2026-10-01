//! The camera a frame renders through.
//!
//! # Responsibilities
//!
//! - Define [`CameraComponent`]: the perspective description the renderer
//!   builds its projection matrix from.
//!
//! # Design
//!
//! Shared and persistable like the other scene components, so the editor edits
//! its fields live and the host carries them across reload generations. The
//! shared name is pinned rather than derived from the module path; see
//! [`crate::components`].
//!
//! **Renderer contract.** [`crate::frame::RenderFrame`] embeds the camera it
//! renders through, so this component stays in `pill_renderer_api` for every
//! renderer, unlike the renderer-specific components beside it.

// External crates
use pill_engine::{PillComponent, World};
use serde::{Deserialize, Serialize};

/// A perspective camera, described so the renderer can build its projection
/// matrix from it and its view matrix from the entity's transform.
///
/// A frame renders through one enabled camera: the highest `priority` wins,
/// and equal priorities go to the lowest entity id, so the choice cannot
/// wander with traversal order. Impossible projection values fall back to the
/// defaults when read, which keeps a mistyped field of view or near/far pair
/// from becoming NaNs in the matrix.
#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PillComponent)]
// Pinned to the name this type carried before it moved into `components/`. A
// derived name follows `module_path!()`, so re-deriving it would register a
// different component and orphan every column a live world holds for this one.
#[pill(
    shared = "pill_master_renderer::component::CameraComponent",
    persistable
)]
pub struct CameraComponent {
    /// Whether the camera may be picked for a frame; a disabled one stays in
    /// the world without ever rendering.
    pub enabled: bool,
    /// Selection weight among the scene's cameras; the highest wins.
    pub priority: i32,
    /// Vertical field of view in degrees; values outside `(0, 180)` fall back
    /// to the default when read.
    pub vertical_fov: f32,
    /// Distance to the near clip plane, in world units.
    pub near: f32,
    /// Distance to the far clip plane, in world units.
    pub far: f32,
}

impl Default for CameraComponent {
    fn default() -> Self {
        Self {
            enabled: true,
            priority: 0,
            vertical_fov: 60.0,
            near: 0.1,
            far: 1000.0,
        }
    }
}

/// Registers [`CameraComponent`] with the world, field layout included.
///
/// The derive generates `__pill_register_CameraComponent` as a private item of
/// this module, so [`crate::components::register_contract_components`] reaches
/// it through here rather than by path.
pub(crate) fn register(world: &mut World) {
    __pill_register_CameraComponent(world);
}
