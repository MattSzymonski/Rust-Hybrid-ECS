//! One spatial sound source and one listener, preserved across project reloads.

use pill_audio::{AudioListenerComponent, AudioLoadQueue, AudioSourceComponent};
use pill_engine::{Engine, Position, Query};

const SOUND_NAME: &str = "sound_1";
// Embed the project asset so development and shipping builds need no working-directory setup.
const SOUND_BYTES: &[u8] = include_bytes!("../assets/audio/sound_1.mp3");

pub(crate) fn initialize(engine: &mut Engine) -> Result<(), &'static str> {
    // The host initializes pill_audio from project_settings.yaml before the project.
    let Some(loads) = engine.world_mut().get_resource_mut::<AudioLoadQueue>() else {
        // Settings can disable audio (including in headless benchmark projects).
        return Ok(());
    };
    loads.load(SOUND_NAME, SOUND_BYTES);

    let has_listener = Query::<&AudioListenerComponent>::new(engine.world_mut())
        .iter_mut()
        .next()
        .is_some();
    if !has_listener {
        engine
            .world_mut()
            .create_entity()
            .with(Position { x: 400.0, y: 300.0 })
            .with(AudioListenerComponent::default())
            .build()
            .map_err(|_| "could not create the audio listener")?;
    }

    let has_source = Query::<&AudioSourceComponent>::new(engine.world_mut())
        .iter_mut()
        .any(|source| source.sound_name.as_str() == SOUND_NAME);
    if !has_source {
        engine
            .world_mut()
            .create_entity()
            // Keep the emitter near the listener: spatial attenuation uses world units.
            .with(Position { x: 400.0, y: 300.0 })
            .with(AudioSourceComponent::playing(SOUND_NAME))
            .build()
            .map_err(|_| "could not create the audio source")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pill_audio::Sound;

    #[test]
    fn audio_is_disabled_without_the_configured_extension() {
        let mut engine = Engine::new();
        initialize(&mut engine).unwrap();
        assert!(engine.world().get_resource::<AudioLoadQueue>().is_none());
        assert!(engine.is_system_enabled("pill_audio").is_none());
    }

    #[test]
    fn bundled_mp3_decodes() {
        let sound = Sound::from_bytes(std::path::Path::new("sound_1.mp3"), SOUND_BYTES.to_vec());
        assert!(
            sound.decoder().is_some(),
            "the bundled MP3 must be playable"
        );
    }

    #[test]
    fn reinitialization_keeps_one_source_and_listener() {
        let mut engine = Engine::new();
        engine.world_mut().register_component::<Position>();
        // Simulate the host initializing the configured extension first.
        pill_audio::register(&mut engine);
        initialize(&mut engine).unwrap();
        initialize(&mut engine).unwrap();
        assert_eq!(
            Query::<&AudioListenerComponent>::new(engine.world_mut())
                .iter_mut()
                .count(),
            1
        );
        let mut query = Query::<&AudioSourceComponent>::new(engine.world_mut());
        let sources: Vec<_> = query.iter_mut().map(|source| *source).collect();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].sound_name.as_str(), SOUND_NAME);
        assert!(sources[0].play_on_awake);
    }
}
