//! One spatial sound source and one listener, preserved across project reloads.
//!
//! # Responsibilities
//!
//! - Register the bundled sound in the asset store when the audio module is
//!   present, and keep the handle the source plays.
//! - Create the listener and the playing source once, so a reload that runs
//!   `init` again finds them instead of adding more.

// External crates
use pill_audio::{AudioListenerComponent, AudioLoadQueue, AudioSourceComponent, Sound};
use pill_engine::{AssetManager, Engine, Handle, Position, Query};

const SOUND_NAME: &str = "sound_1";
// Embed the project asset so development and shipping builds need no working-directory setup.
const SOUND_BYTES: &[u8] = include_bytes!("../assets/audio/sound_1.mp3");

/// Register the sound and create its listener and source, unless they already
/// exist. Does nothing when the audio module is not loaded.
///
/// # Errors
///
/// Returns a message when the sound, the listener or the source cannot be
/// created.
pub(crate) fn initialize(engine: &mut Engine) -> Result<(), &'static str> {
    // The host initializes pill_audio from project_settings.yaml before the project,
    // and settings can disable audio (including in headless benchmark projects):
    // the module installs the queue when it loads, so its absence is the one
    // signal a project has.
    if engine.world().get_resource::<AudioLoadQueue>().is_none() {
        return Ok(());
    }
    // `Sound` is a shared asset type, so the handle taken here is the one the
    // module's playback system resolves even though the two DLLs hold their
    // own copy of the crate: one pinned name, one asset column.
    let sound: Handle<Sound> = {
        let assets = engine
            .world_mut()
            .get_resource_mut::<AssetManager>()
            .ok_or("the engine always owns an AssetManager")?;
        if let Some(sound) = assets.handle_by_name::<Sound>(SOUND_NAME) {
            sound
        } else {
            let path = std::path::Path::new(SOUND_NAME);
            assets
                .add_named(SOUND_NAME, Sound::from_bytes(path, SOUND_BYTES.to_vec()))
                .map_err(|_| "could not register the bundled sound")?
        }
    };

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
        .any(|source| source.sound == sound);
    if !has_source {
        engine
            .world_mut()
            .create_entity()
            // Keep the emitter near the listener: spatial attenuation uses world units.
            .with(Position { x: 400.0, y: 300.0 })
            .with(AudioSourceComponent::playing(sound))
            .build()
            .map_err(|_| "could not create the audio source")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pill_audio::Sound;
    use pill_engine::Asset;

    #[test]
    fn audio_is_disabled_without_the_configured_extension() {
        let mut engine = Engine::new();
        initialize(&mut engine).unwrap();
        assert!(engine.world().get_resource::<AudioLoadQueue>().is_none());
        assert!(engine.is_system_enabled("pill_audio").is_none());
    }

    /// The project registers the renderer data and loads the audio module,
    /// whose imported asset types must not claim the same extension.
    #[test]
    fn every_imported_asset_type_registers_without_a_conflict() {
        let mut engine = Engine::new();
        pill_master_renderer_data::register(&mut engine);
        pill_audio::register(&mut engine);
        assert!(engine.world_mut().take_registration_error().is_none());

        let registry = engine
            .world()
            .get_resource::<pill_engine::ImportRegistry>()
            .expect("the modules record their imported types");
        for (extension, type_name) in [
            ("png", "pill_master_renderer::assets::Texture"),
            ("jpg", "pill_master_renderer::assets::Texture"),
            ("obj", "pill_master_renderer::assets::Mesh"),
            ("mp3", "pill_audio::Sound"),
        ] {
            assert_eq!(registry.type_for_extension(extension), Some(type_name));
        }
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
        assert!(sources[0].play_on_awake);
        // The source names the one sound the store holds, which is what makes
        // the second `initialize` adopt it instead of adding another.
        let registered = engine
            .world()
            .get_resource::<AssetManager>()
            .expect("the engine owns the asset store")
            .handle_by_name::<Sound>(SOUND_NAME)
            .expect("the sound is registered");
        assert_eq!(sources[0].sound, registered);
    }

    /// The shared name is what gives the project and the module DLL one asset
    /// column, and it is the spelling every `.meta` file records.
    #[test]
    fn the_sound_type_keeps_its_pinned_name() {
        assert_eq!(Sound::shared_name(), Some("pill_audio::Sound"));
    }
}
