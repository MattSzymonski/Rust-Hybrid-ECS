//! The [`AudioListenerComponent`] component: where the world is heard from.
//!
//! # Responsibilities
//!
//! - Declares the component marking the entity whose position the listener
//!   hears from.
//!
//! # Design
//!
//! Carries its own `facing_degrees` rather than reading a rotation component,
//! because this engine's `Position` is 2D and has no orientation. When a 3D
//! transform arrives, this field is what it replaces.

// External crates
use pill_engine::PillComponent;
use serde::{Deserialize, Serialize};

// =============================================================================
// AudioListenerComponent
// =============================================================================

/// Marks the entity whose position the listener hears from.
///
/// The first enabled listener found wins; there is one pair of ears. An
/// entity also needs a [`Position`] for the listener to have a location.
#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PillComponent)]
#[pill(persistable, shared)]
pub struct AudioListenerComponent {
    /// Whether this listener is the active one.
    pub enabled: bool,
    /// Direction the listener faces, in degrees counter-clockwise from +X.
    ///
    /// Stands in for the old engine's rotation component: this engine's
    /// `Position` is 2D and carries no orientation, so a listener that can
    /// turn needs to say so itself.
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
