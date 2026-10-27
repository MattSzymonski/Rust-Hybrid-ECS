//! Extension providing sound playback.
//!
//! # Responsibilities
//!
//! - Defines the [`Sound`] asset: encoded audio bytes held by the
//!   [`AssetManager`](pill_engine::AssetManager) and decoded per playback.
//! - Defines the [`AudioListenerComponent`] and [`AudioSourceComponent`]
//!   components, and the [`AudioManager`] resource owning the output stream
//!   and its sink pools.
//! - Registers the [`audio_system`] that drives playback each frame.
//!
//! # Design
//!
//! **A sound is an asset, not a resource.** A world holds many sounds, so
//! [`Sound`] implements [`Asset`](pill_engine::Asset) and is addressed by
//! `Handle<Sound>`. The handle is generational: unloading a sound leaves every
//! [`AudioSourceComponent`] pointing at it resolving to `None` next frame,
//! instead of the asset having to clear every component that names it.
//!
//! **The audio manager is a resource** because there is exactly one output
//! device, and its rodio stream must never be dropped while sinks play - the
//! lifetime a resource has.
//!
//! **Commands carry intent.** A caller mutating a component cannot reach the
//! manager, so intent is stored as a plain [`AudioCommand`] field and drained
//! by [`audio_system`]. The component stays `#[repr(C)]` plain data, which is
//! what lets it survive a hot reload and be read from C#.
//!
//! ## Spatial audio and the 2D world
//!
//! `Position` is 2D while rodio's spatial sink wants a 3D point, so a
//! [`Position`] is lifted to `(x, y, 0.0)`, and [`AudioListenerComponent`]
//! carries its own `facing_degrees` rather than reading a rotation component
//! this engine does not have. [`ear_positions`] is the one conversion between
//! the two spaces.

// The build script scans this crate and emits one address entry per function
// into `function_inventory.rs`; the `include!` is what makes every function
// resolvable by qualified path with nothing in this file annotated. It lives
// at the crate root because that is where the generated paths are rooted.
include!(concat!(env!("OUT_DIR"), "/function_inventory.rs"));

// External crates
use pill_engine::*;

// =============================================================================
// Modules
// =============================================================================

/// [`AudioCommand`]: what a source has been asked to do next.
pub mod audio_command;
/// The [`AudioListenerComponent`] component.
pub mod audio_listener_component;
/// Shared project-to-extension sound loading requests.
pub mod audio_load_queue;
/// The [`AudioManager`] resource: output device and sink pools.
pub mod audio_manager;
/// The [`AudioSourceComponent`] component.
pub mod audio_source_component;
/// The per-frame system driving playback.
pub mod audio_system;
/// Where a listener's ears sit in the world.
pub mod listener_geometry;
/// The [`Sound`] asset.
pub mod sound;
/// [`SoundType`]: whether a sound is positional.
pub mod sound_type;

// =============================================================================
// Re-exports
// =============================================================================

// The module that declares a type is an implementation detail; a caller names
// `pill_audio::AudioSourceComponent` regardless of which file it lives in.
pub use audio_command::AudioCommand;
pub use audio_listener_component::AudioListenerComponent;
pub use audio_load_queue::AudioLoadQueue;
pub use audio_manager::{
    AudioManager, DEFAULT_AMBIENT_SINK_COUNT, DEFAULT_SPATIAL_SINK_COUNT, EAR_SEPARATION,
};
pub use audio_source_component::{AudioSourceComponent, NO_SINK};
pub use audio_system::audio_system;
pub use listener_geometry::ear_positions;
pub use sound::{Sound, SoundImportSettings, SoundLoadError, SUPPORTED_AUDIO_FORMATS};
pub use sound_type::SoundType;

// =============================================================================
// Registration
// =============================================================================

/// Registers this module's components, resource and system with the engine.
///
/// Called directly by a monolithic build; the generated
/// `host_module_pill_audio` wrapper carries it as `pill_module_init` for the
/// host to find in a loaded DLL.
///
/// Installing the [`AudioManager`] is conditional: a machine with no audio
/// device gets the components and the system but no manager, and the system
/// then only drains queued sound loads. That is what keeps this module
/// loadable on a CI runner.
#[pill_module]
pub fn register(engine: &mut Engine) -> u32 {
    if engine.world().get_resource::<AudioLoadQueue>().is_none() {
        engine
            .world_mut()
            .insert_resource(AudioLoadQueue::default());
    }
    // Through the derives' own registrars, so each component arrives with its
    // field layout: that is what the Inspector's rows and the managed codegen
    // read, and a component without one is an opaque blob to both.
    audio_listener_component::register(engine.world_mut());
    audio_source_component::register(engine.world_mut());
    // Declare the asset type this module stores. Sounds outlive a reload of
    // this DLL - the world's asset store does - so the column's per-type table
    // has to be re-pointed at the generation still mapped. Registering it as
    // an imported asset does that and also lets sound files import by
    // extension.
    engine.world_mut().register_imported_asset::<Sound>();

    // Only build the device once. Registration runs again on every reload, and
    // replacing a live manager would cut off whatever is playing.
    if engine.world().get_resource::<AudioManager>().is_none() {
        match AudioManager::with_defaults() {
            Some(manager) => {
                engine.world_mut().insert_resource(manager);
            }
            None => {
                pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    "no audio output device; pill_audio loaded without playback"
                );
            }
        }
    }
    engine.register_system("pill_audio", audio_system);

    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ECS,
        ambient_sinks = DEFAULT_AMBIENT_SINK_COUNT,
        spatial_sinks = DEFAULT_SPATIAL_SINK_COUNT,
        "pill_audio module registered"
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use pill_engine::{Engine, Handle};

    /// An audio source's sound is a typed asset reference: it reads back
    /// naming the shared sound type - what the Inspector draws as an asset row
    /// and the picker lists - and a handle writes back into the component the
    /// playback system reads.
    #[test]
    fn the_audio_sources_sound_is_a_writable_asset_reference() {
        let mut engine = Engine::new();
        assert_eq!(register(&mut engine), 0);
        let world = engine.world_mut();
        let entity = world
            .create_entity()
            .with(AudioSourceComponent::new(Handle::from_raw(2, 1)))
            .build()
            .unwrap();
        let name =
            pill_engine::component::ComponentRegistry::registered_name::<AudioSourceComponent>();
        assert_eq!(
            world.read_component_field(entity, &name, "sound").unwrap(),
            pill_engine::FieldValue::Asset {
                asset_type: Some("pill_audio::Sound".to_owned()),
                index: 2,
                generation: 1,
            }
        );

        world
            .write_component_field(
                entity,
                &name,
                "sound",
                pill_engine::FieldValue::Asset {
                    asset_type: Some("pill_audio::Sound".to_owned()),
                    index: 4,
                    generation: 0,
                },
            )
            .expect("another sound's handle writes");
        assert_eq!(
            world
                .get_component::<AudioSourceComponent>(entity)
                .unwrap()
                .sound,
            Handle::from_raw(4, 0)
        );

        let refused = world.write_component_field(
            entity,
            &name,
            "sound",
            pill_engine::FieldValue::Asset {
                asset_type: Some("pill_master_renderer::assets::Mesh".to_owned()),
                index: 0,
                generation: 0,
            },
        );
        assert!(
            matches!(
                refused,
                Err(pill_engine::component_field::ComponentFieldError::AssetTypeMismatch { .. })
            ),
            "{refused:?}"
        );
    }
}
