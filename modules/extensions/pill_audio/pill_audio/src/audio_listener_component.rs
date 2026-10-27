//! The [`AudioListenerComponent`] component: where the world is heard from.
//!
//! # Responsibilities
//!
//! - Declares the component marking the entity whose position the listener
//!   hears from.
//!
//! # Design
//!
//! Carries its own `facing_degrees` rather than reading the transform's
//! rotation, because the rotation is a quaternion and panning needs one yaw
//! angle; this field is the listener's own.

// External crates
use pill_engine::PillComponent;
use serde::{Deserialize, Serialize};

// =============================================================================
// AudioListenerComponent
// =============================================================================

/// Marks the entity whose position the listener hears from.
///
/// The first enabled listener found wins; there is one pair of ears. An
/// entity also needs a [`TransformComponent`](pill_engine::TransformComponent)
/// for the listener to have a location.
#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PillComponent)]
#[pill(persistable, shared)]
pub struct AudioListenerComponent {
    /// Whether this listener is the active one.
    pub enabled: bool,
    /// Direction the listener faces, in degrees counter-clockwise from +X.
    ///
    /// The transform's rotation is a quaternion and panning needs a single
    /// yaw, so a listener that can turn says so itself.
    pub facing_degrees: f32,
}

impl core::default::Default for AudioListenerComponent {
    fn default() -> Self {
        Self {
            enabled: true,
            facing_degrees: 0.0,
        }
    }
}

// --- Registration ---

/// Registers [`AudioListenerComponent`] with the world, field layout included.
///
/// The derive generates `__pill_register_AudioListenerComponent` as a private
/// item of this module, so [`crate::register`] reaches it through here rather
/// than by path. Without the layout the Inspector and the managed codegen see
/// an opaque blob instead of the two rows this component has.
pub(crate) fn register(world: &mut pill_engine::World) {
    __pill_register_AudioListenerComponent(world);
}
