//! The per-frame system driving every [`AudioSourceComponent`].
//!
//! # Responsibilities
//!
//! - Places the active listener's ears.
//! - Carries out each source's pending [`AudioCommand`].
//! - Follows spatial sources with their emitter, and reclaims finished sinks.
//!
//! # Design
//!
//! Takes its state as ordinary system parameters rather than `&mut Engine`,
//! so the scheduler derives the access pattern from the signature and
//! serialises this against anything else touching the same resources.
//!
//! Reclaiming finished sinks is what makes a fixed pool workable: without it
//! a sound that ended would hold its sink until the entity was destroyed.

// External crates
use pill_engine::{AssetManager, Position, Query, Res, ResMut, SystemError};

// Current crate
use crate::audio_command::AudioCommand;
use crate::audio_listener_component::AudioListenerComponent;
use crate::audio_manager::AudioManager;
use crate::audio_source_component::{AudioSourceComponent, NO_SINK};
use crate::listener_geometry::ear_positions;
use crate::sound::Sound;
use crate::sound_type::SoundType;

// =============================================================================
// audio_system
// =============================================================================

/// Drives every [`AudioSourceComponent`] once per frame.
///
/// Four things happen, in this order:
///
/// 1. the active listener's ears are placed, so panning reflects this frame;
/// 2. each source's pending [`AudioCommand`] is carried out;
/// 3. each playing spatial source's emitter is moved to its entity;
/// 4. sinks whose sound has finished are returned to the pool.
///
/// Step 4 is what makes a fixed pool workable: without it a sound that ended
/// would hold its sink until the entity was destroyed.
///
/// Takes its state as ordinary system parameters, so the scheduler derives the
/// access pattern from the signature and serialises this against anything else
/// touching the same resources - which is also why the manager and the asset
/// store arrive separately rather than through the world.
///
/// Does nothing when no [`AudioManager`] is installed, which is the headless
/// case; see [`AudioManager::new`].
pub fn audio_system(
    mut manager: ResMut<AudioManager>,
    assets: Res<AssetManager>,
    mut listeners: Query<(&AudioListenerComponent, &Position)>,
    mut sources: Query<(&mut AudioSourceComponent, &Position)>,
) -> Result<(), SystemError> {
    // Step 1: find the active listener. One pair of ears, so the first enabled
    // listener wins and the rest are ignored.
    let mut ears = None;
    for (listener, position) in listeners.iter_mut() {
        if listener.enabled {
            ears = Some(ear_positions(
                (position.x, position.y),
                listener.facing_degrees,
            ));
            break;
        }
    }

    // Absent on a machine with no audio device; the components still exist and
    // their commands simply go unserviced.
    let Some(mut manager) = manager.get_mut() else {
        return Ok(());
    };
    let Some(assets) = assets.get() else {
        return Ok(());
    };

    if let Some((left, right)) = ears {
        manager.set_ears(left, right);
    }

    for (mut source, position) in sources.iter_mut() {
        // `play_on_awake` is a one-shot: clearing it here means a reload,
        // which re-runs registration but keeps components, does not restart
        // every sound in the world.
        if source.play_on_awake {
            source.play_on_awake = false;
            if source.command == AudioCommand::None {
                source.command = AudioCommand::Play;
            }
        }

        let sound_type = source.sound_type;
        let command = std::mem::replace(&mut source.command, AudioCommand::None);

        // Step 2: carry out the pending command.
        match command {
            AudioCommand::Play => {
                // A source already holding a sink is restarted rather than
                // layered: two decoders on one sink play back to back.
                if source.has_sink() {
                    manager.release(sound_type, source.sink);
                    source.sink = NO_SINK;
                }
                let sound = assets.get_by_name::<Sound>(source.sound_name.as_str());
                if let Some(sound) = sound {
                    if let Some(sink) = manager.acquire(sound_type) {
                        if manager.start(sound_type, sink, sound, source.volume) {
                            source.sink = sink;
                            source.is_playing = true;
                        } else {
                            // Undecodable bytes: give the sink straight back
                            // rather than holding one that plays nothing.
                            manager.release(sound_type, sink);
                            source.is_playing = false;
                        }
                    }
                }
            }
            AudioCommand::Pause => {
                if source.has_sink() {
                    manager.pause(sound_type, source.sink);
                    source.is_playing = false;
                }
            }
            AudioCommand::Stop => {
                if source.has_sink() {
                    manager.release(sound_type, source.sink);
                    source.sink = NO_SINK;
                    source.is_playing = false;
                }
            }
            AudioCommand::None => {}
        }

        // Step 3: follow the entity with the emitter.
        if source.has_sink() && source.is_playing {
            if sound_type == SoundType::Spatial {
                manager.set_emitter(source.sink, [position.x, position.y, 0.0]);
            }
            manager.set_volume(sound_type, source.sink, source.volume);
        }

        // Step 4: reclaim a sink whose sound has ended.
        if source.has_sink() && source.is_playing && manager.is_finished(sound_type, source.sink) {
            manager.release(sound_type, source.sink);
            source.sink = NO_SINK;
            source.is_playing = false;
        }
    }

    Ok(())
}
