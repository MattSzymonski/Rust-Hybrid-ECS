//! Shared requests for the audio extension to load project-provided sound bytes.
//!
//! # Responsibilities
//!
//! - Carry encoded sound bytes from a project to the running playback DLL,
//!   which turns them into [`Sound`](crate::Sound) assets on its next frame.
//! - Coalesce repeated loads of one playback name.
//!
//! # Design
//!
//! [`Sound`](crate::Sound) is a shared asset type, so a project that links the
//! crate can also register a sound itself and take the handle directly - that
//! is what `examples/project_rs` does with its embedded MP3. The queue stays
//! for bytes a project hands over at runtime without naming the type: the
//! module drains it, so the project never has to know when loading happens.

// Standard library
use std::collections::BTreeMap;

/// Pending sound loads, by playback name.
#[repr(C)]
#[derive(Default)]
pub struct AudioLoadQueue {
    pub(crate) sounds: BTreeMap<String, Vec<u8>>,
}

// The trait's full path, not an import: the hot-patch inventory is generated at
// the crate root and cannot see this module's `use` lines.
impl pill_engine::Resource for AudioLoadQueue {
    fn shared_name() -> Option<&'static str> {
        Some("pill_audio::AudioLoadQueue")
    }
}

impl AudioLoadQueue {
    /// Queue an encoded sound under its playback name. Repeated requests coalesce.
    pub fn load(&mut self, name: &str, bytes: &[u8]) {
        self.sounds.insert(name.to_owned(), bytes.to_vec());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{audio_system, AudioListenerComponent, AudioSourceComponent, Sound};
    use pill_engine::{AssetManager, Engine, Position};

    #[test]
    fn playback_system_consumes_requests_without_an_audio_device() {
        let mut engine = Engine::new();
        engine.world_mut().register_component::<Position>();
        engine
            .world_mut()
            .register_component::<AudioListenerComponent>();
        engine
            .world_mut()
            .register_component::<AudioSourceComponent>();
        let mut queue = AudioLoadQueue::default();
        queue.load("test", &[1, 2, 3]);
        engine.world_mut().insert_resource(queue);
        engine.register_system("audio", audio_system);
        engine.process_frame().unwrap();
        assert!(engine.system_failures().is_empty());
        assert!(engine
            .world()
            .get_resource::<AudioLoadQueue>()
            .unwrap()
            .sounds
            .is_empty());
        let assets = engine.world().get_resource::<AssetManager>().unwrap();
        assert_eq!(
            assets.get_by_name::<Sound>("test").unwrap().bytes(),
            &[1, 2, 3]
        );
    }
}
