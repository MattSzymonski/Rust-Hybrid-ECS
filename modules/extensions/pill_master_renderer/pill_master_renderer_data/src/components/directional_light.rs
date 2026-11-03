//! The directional light a scene declares.
//!
//! # Responsibilities
//!
//! - Define [`DirectionalLightComponent`]: the colour and strength of the
//!   scene's key light.
//!
//! # Design
//!
//! The component is shared and persistable, so a scene keeps it and the C#
//! side sees the same type. No shipped pass reads it - each pass writes its
//! own lights into its shaders - so the values are stored but change nothing
//! that is drawn. The shared name is written out in full rather than derived
//! from the module path; [`crate::components`] explains why.

// External crates
use pill_engine::{PillComponent, World};
use serde::{Deserialize, Serialize};

// =============================================================================
// DirectionalLightComponent
// =============================================================================

/// A directional light, described by the colour it emits and its strength.
///
/// A project sets one to record the scene's key light in a standard shape, and
/// the editor edits its fields live. The shipped passes do not read it - each
/// writes its own lights into its shaders - so the value is saved with the
/// scene and never drawn.
#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PillComponent)]
// The name is written out so the component's identity does not follow the
// module path. A derived name is built from `module_path!()`, so deriving it
// again would register a second component, and the data a running world
// already holds under the first one would never be found again.
#[pill(
    shared = "pill_master_renderer::component::DirectionalLightComponent",
    persistable
)]
pub struct DirectionalLightComponent {
    /// Linear RGB colour the light emits: red, green, blue, one value each.
    #[pill(color)]
    pub color: [f32; 3],
    /// Brightness of the light, multiplied into each colour channel.
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

// =============================================================================
// Registration
// =============================================================================

/// Registers [`DirectionalLightComponent`] with the world, together with its
/// field layout.
///
/// The derive generates `__pill_register_DirectionalLightComponent` as a
/// private item of this module, so
/// [`crate::components::register_components`] cannot call it by path and calls
/// this function instead. The layout is what the editor reads to show the
/// fields and what the C# code generator uses to type them.
pub(crate) fn register(world: &mut World) {
    __pill_register_DirectionalLightComponent(world);
}
