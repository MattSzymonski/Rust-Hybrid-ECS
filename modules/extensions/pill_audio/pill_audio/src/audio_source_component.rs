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
//! be mirrored into C#. The sound is held as a [`Handle<Sound>`], like a mesh
//! or a material: `Handle` is `#[repr(C)]` two-integer data with no allocation,
//! so it crosses the module boundary as safely as the fixed-size name it
//! replaced, and it names one loaded sound instead of a string somebody has to
//! resolve again on every play.

// External crates
use pill_engine::Handle;
use pill_engine::PillComponent;
use serde::{Deserialize, Serialize};

// Current crate
use crate::audio_command::AudioCommand;
use crate::sound::Sound;
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
/// The sound is a [`Handle<Sound>`], the same reference every other component
/// uses for its assets. A handle that no longer resolves - its sound was
/// unloaded, or the asset store was replaced - plays nothing, which is what
/// makes unloading a sound safe.
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
    /// The sound this source plays.
    ///
    /// [`Handle::INVALID`] until it is pointed at one, which plays nothing.
    pub sound: Handle<Sound>,
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
            sound: Handle::INVALID,
        }
    }
}

impl AudioSourceComponent {
    /// A source that will play `sound` when the world next ticks.
    pub fn playing(sound: Handle<Sound>) -> Self {
        Self {
            play_on_awake: true,
            sound,
            ..Self::default()
        }
    }

    /// A source bound to `sound` but silent until [`Self::play`] is called.
    pub fn new(sound: Handle<Sound>) -> Self {
        Self {
            sound,
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
    /// so continuing would play the previous sound under the new one.
    pub fn set_sound(&mut self, sound: Handle<Sound>) {
        if self.sound != sound {
            self.sound = sound;
            self.command = AudioCommand::Stop;
        }
    }

    /// Whether a sink is currently assigned to this source.
    pub fn has_sink(&self) -> bool {
        self.sink != NO_SINK
    }
}

// =============================================================================
// Registration
// =============================================================================

/// Registers [`AudioSourceComponent`] with the world, field layout included.
///
/// The derive generates `__pill_register_AudioSourceComponent` as a private
/// item of this module, so [`crate::register`] reaches it through here rather
/// than by path. The layout is what turns the `sound` handle into an asset row
/// in the Inspector and into a typed field in the managed mirror.
pub(crate) fn register(world: &mut pill_engine::World) {
    __pill_register_AudioSourceComponent(world);
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A handle is plain two-integer data, which is what lets the component
    /// cross the module boundary; `pill_engine` keeps that layout.
    #[test]
    fn the_sound_is_a_plain_handle() {
        let sound = Handle::from_raw(3, 1);
        let source = AudioSourceComponent::new(sound);
        assert_eq!(source.sound, sound);
        assert_eq!(source.sound.index(), 3);
        assert_eq!(
            AudioSourceComponent::default().sound,
            Handle::<Sound>::INVALID
        );
    }

    /// Pointing a source at a new sound stops the old one, because the sink
    /// still holds a decoder over the previous bytes.
    #[test]
    fn changing_the_sound_requests_a_stop() {
        let mut source = AudioSourceComponent::new(Handle::from_raw(1, 0));
        assert_eq!(source.command, AudioCommand::None);
        source.set_sound(Handle::from_raw(2, 0));
        assert_eq!(source.sound, Handle::from_raw(2, 0));
        assert_eq!(source.command, AudioCommand::Stop);
    }

    /// Setting the same sound again is not a change, so playback continues.
    #[test]
    fn setting_the_same_sound_is_a_no_op() {
        let sound = Handle::from_raw(1, 0);
        let mut source = AudioSourceComponent::new(sound);
        source.set_sound(sound);
        assert_eq!(source.command, AudioCommand::None);
    }

    /// The command field records the last intent expressed.
    #[test]
    fn commands_replace_one_another() {
        let mut source = AudioSourceComponent::new(Handle::from_raw(1, 0));
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
        let sound = Handle::from_raw(7, 2);
        let source = AudioSourceComponent::playing(sound);
        assert!(source.play_on_awake);
        assert_eq!(source.sound, sound);
    }
}
