//! [`AudioCommand`]: what an audio source has been asked to do.
//!
//! # Responsibilities
//!
//! - Declares the pending-intent vocabulary an
//!   [`AudioSourceComponent`](crate::AudioSourceComponent) records and the system drains.
//!
//! # Design
//!
//! This is what replaces the old engine's deferred-update requests. A caller
//! mutating a component cannot reach the [`AudioManager`](crate::AudioManager),
//! so the intent is written here as plain data and carried out by
//! [`audio_system`](crate::audio_system), which holds both.

// External crates
use serde::{Deserialize, Serialize};

// =============================================================================
// AudioCommand
// =============================================================================

/// What an [`AudioSourceComponent`] has been asked to do, pending the next frame.
///
/// This replaces the old engine's deferred-update requests. A caller mutating
/// a component cannot reach the [`AudioManager`], so the intent is recorded
/// here and [`audio_system`] - which holds both - carries it out. One slot
/// rather than a queue: the commands are idempotent state changes, and a
/// caller that plays then stops within one frame means the latter.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AudioCommand {
    /// Nothing pending.
    #[default]
    None = 0,
    /// Begin playback from the start.
    Play = 1,
    /// Halt playback, keeping the position.
    Pause = 2,
    /// Halt playback and release the sink.
    Stop = 3,
}
