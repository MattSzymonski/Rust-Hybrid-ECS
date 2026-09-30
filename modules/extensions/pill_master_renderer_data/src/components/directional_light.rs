//! The directional light a scene declares.
//!
//! # Responsibilities
//!
//! - Define [`DirectionalLightComponent`]: the colour and strength of the
//!   scene's key light.
//!
//! # Design
//!
//! Shared and persistable, and edited live through the editor's field layout.
//! Nothing reads it yet, so it is carried as scene contract only - see the
//! type's own note. The shared name is pinned rather than derived from the
//! module path; see [`crate::components`].

// External crates
use pill_engine::{PillComponent, World};
use serde::{Deserialize, Serialize};

/// A directional light, described by the colour it emits and its strength.
///
/// Projects declare one to light the scene, and the editor edits its fields
/// live through the shared layout. The shipped passes do not read it yet -
/// their light rigs are baked into the shaders - so today it travels the
/// scene contract and nothing more.
#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PillComponent)]
// Pinned to the name this type carried before it moved into `components/`. A
// derived name follows `module_path!()`, so re-deriving it would register a
// different component and orphan every column a live world holds for this one.
#[pill(
    shared = "pill_master_renderer::component::DirectionalLightComponent",
    persistable
)]
pub struct DirectionalLightComponent {
    /// Linear RGB colour the light emits, one channel per element.
    pub color: [f32; 3],
    /// Scalar multiplier applied to each colour channel.
    pub intensity: f32,
}

impl Default for DirectionalLightComponent {
    fn default() -> Self {
        Self {
            color: [1.0; 3],
            intensity: 3.0,
        }
    }
}

/// Registers [`DirectionalLightComponent`] with the world, field layout
/// included.
///
/// The derive generates `__pill_register_DirectionalLightComponent` as a
/// private item of this module, so
/// [`crate::components::register_components`] reaches it through here rather
/// than by path.
pub(crate) fn register(world: &mut World) {
    __pill_register_DirectionalLightComponent(world);
}
