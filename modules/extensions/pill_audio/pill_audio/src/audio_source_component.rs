//! The [`AudioSourceComponent`] component: an entity that plays a sound.
//!
//! # Responsibilities
//!
//! - Declares the component holding playback intent and the state
//!   [`audio_system`](crate::audio_system) writes back to it.
//!
//! # Design
//!
//! Every field is plain data, so the component survives a hot reload and can
//! be mirrored into C#. That constraint is why the sound is named with a
//! [`FixedString64`] rather than held as a `Handle<Sound>` or a `String`: the
//! component is copied between archetypes and lives in host-owned memory
//! while this module is a separately loaded library.

// External crates
use pill_core::FixedString64;
use pill_engine::PillComponent;
use serde::{Deserialize, Serialize};

// Current crate
use crate::audio_command::AudioCommand;
use crate::sound_type::SoundType;

// =============================================================================
// AudioSourceComponent
// =============================================================================

/// Plays a [`Sound`] from an entity.
///
/// Holds the *intent* to play; [`audio_system`] performs it and records the
/// result back here. Every field is plain data, so the component survives a
/// hot reload and can be mirrored into C#.
///
/// The sound is named rather than handle-typed because `Handle<Sound>` is not
/// `#[repr(C)]`-stable across artifacts: a name is resolved through
/// [`AssetManager::handle_by_name`](pill_engine::AssetManager::handle_by_name)
/// each time playback starts, which also means unloading and reloading a sound
/// under the same name does the right thing.
#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PillComponent)]
#[pill(persistable, shared)]
pub struct AudioSourceComponent {
    /// Which sink pool to draw from.
    pub sound_type: SoundType,
    /// Playback volume, where 1.0 is the sound's own level.
    pub volume: f32,
    /// Whether to start playing as soon as the sound resolves.
    ///
    /// Cleared once acted on, so a reload does not restart the sound.
    pub play_on_awake: bool,
    /// What the source has been asked to do next.
    pub command: AudioCommand,
    /// Whether a sink is currently playing this source.
    ///
    /// Written by [`audio_system`]; treat as read-only.
    pub is_playing: bool,
    /// Index of the sink serving this source, or `NO_SINK`.
    ///
    /// Written by [`audio_system`]; treat as read-only.
    pub sink: u32,
    /// Name the sound is registered under in the
    /// [`AssetManager`](pill_engine::AssetManager).
    ///
    /// A [`FixedString64`] rather than a `String`: the component is copied
    /// between archetypes and lives in host-owned memory while this module is
    /// a separately loaded library, so a heap allocation made by one and freed
    /// by the other is exactly the hazard `#[repr(C)]` plain data avoids.
    pub sound_name: FixedString64,
}

/// Sentinel for [`AudioSourceComponent::sink`] when no sink is held.
pub const NO_SINK: u32 = u32::MAX;

impl core::default::Default for AudioSourceComponent {
    fn default() -> Self {
        Self {
            sound_type: SoundType::Spatial,
            volume: 1.0,
            play_on_awake: false,
            command: AudioCommand::None,
            is_playing: false,
            sink: NO_SINK,
            sound_name: FixedString64::empty(),
        }
    }
}

impl AudioSourceComponent {
    /// A source that will play `name` when the world next ticks.
    pub fn playing(name: &str) -> Self {
        Self {
            play_on_awake: true,
            sound_name: FixedString64::new(name),
            ..Self::default()
        }
    }

    /// A source bound to `name` but silent until [`Self::play`] is called.
    pub fn new(name: &str) -> Self {
        Self {
            sound_name: FixedString64::new(name),
            ..Self::default()
        }
    }

    /// Request playback from the start on the next frame.
    pub fn play(&mut self) {
        self.command = AudioCommand::Play;
    }

    /// Request a pause on the next frame.
    pub fn pause(&mut self) {
        self.command = AudioCommand::Pause;
    }

    /// Request a stop, releasing the sink, on the next frame.
    pub fn stop(&mut self) {
        self.command = AudioCommand::Stop;
    }

    /// Point this source at a different sound.
    ///
    /// Stops whatever is playing: the sink holds a decoder over the old bytes,
    /// so continuing would play the previous sound under the new name.
    pub fn set_sound(&mut self, name: &str) {
        if self.sound_name.as_str() != name {
            self.sound_name = FixedString64::new(name);
            self.command = AudioCommand::Stop;
        }
    }

    /// Whether a sink is currently assigned to this source.
    pub fn has_sink(&self) -> bool {
        self.sink != NO_SINK
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The name field is inline plain data, which is what lets the component
    /// cross the module boundary; `FixedString64` owns the truncation and
    /// UTF-8 rules, tested in `pill_core`.
    #[test]
    fn the_sound_name_is_inline_plain_data() {
        let source = AudioSourceComponent::new("footstep");
        assert_eq!(source.sound_name, "footstep");
        assert!(AudioSourceComponent::default().sound_name.is_empty());
    }

    /// Pointing a source at a new sound stops the old one, because the sink
    /// still holds a decoder over the previous bytes.
    #[test]
    fn changing_the_sound_requests_a_stop() {
        let mut source = AudioSourceComponent::new("footstep");
        assert_eq!(source.command, AudioCommand::None);
        source.set_sound("splash");
        assert_eq!(source.sound_name.as_str(), "splash");
        assert_eq!(source.command, AudioCommand::Stop);
    }

    /// Setting the same sound again is not a change, so playback continues.
    #[test]
    fn setting_the_same_sound_is_a_no_op() {
        let mut source = AudioSourceComponent::new("footstep");
        source.set_sound("footstep");
        assert_eq!(source.command, AudioCommand::None);
    }

    /// The command field records the last intent expressed.
    #[test]
    fn commands_replace_one_another() {
        let mut source = AudioSourceComponent::new("music");
        source.play();
        assert_eq!(source.command, AudioCommand::Play);
        source.pause();
        assert_eq!(source.command, AudioCommand::Pause);
        source.stop();
        assert_eq!(source.command, AudioCommand::Stop);
    }

    /// A fresh source holds no sink, which `has_sink` reports through the
    /// sentinel rather than an `Option` the C# mirror could not read.
    #[test]
    fn a_new_source_holds_no_sink() {
        let source = AudioSourceComponent::default();
        assert!(!source.has_sink());
        assert_eq!(source.sink, NO_SINK);
        assert!(!source.is_playing);
    }

    /// `playing` is the shorthand for "start as soon as the world ticks".
    #[test]
    fn playing_marks_the_source_for_awake_playback() {
        let source = AudioSourceComponent::playing("intro");
        assert!(source.play_on_awake);
        assert_eq!(source.sound_name.as_str(), "intro");
    }
}
