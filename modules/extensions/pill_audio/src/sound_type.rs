//! [`SoundType`]: whether a sound is positional.
//!
//! # Responsibilities
//!
//! - Declares the two playback modes, which select the sink pool a source
//!   draws from.
//!
//! # Design
//!
//! Its own file because both components and the manager name it, and because
//! it is the discriminant that decides which of the two sink pools every
//! playback operation touches.

// External crates
use serde::{Deserialize, Serialize};

// =============================================================================
// SoundType
// =============================================================================

/// How a sound is positioned relative to the listener.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SoundType {
    /// Non-positional: played at a fixed volume regardless of where the
    /// entity is. Music and UI.
    Ambient = 0,
    /// Positional: panned and attenuated by the distance between the entity
    /// and the listener's ears.
    #[default]
    Spatial = 1,
}
