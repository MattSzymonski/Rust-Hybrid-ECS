//! Extension providing sound playback.
//!
//! # Responsibilities
//!
//! - Defines the [`Sound`] asset: decoded-on-demand audio bytes held by the
//!   [`AssetManager`](pill_engine::AssetManager).
//! - Defines the [`AudioListenerComponent`] and [`AudioSourceComponent`] components, and the
//!   [`AudioManager`] resource owning the output stream and its sink pools.
//! - Registers the [`audio_system`] that drives playback each frame.
//!
//! # Design
//!
//! Ported from the old engine's `pill_engine::ecs` audio, and reshaped around
//! three differences in how this engine stores things.
//!
//! **A sound is an asset, not a resource.** The old engine called everything a
//! "resource"; here a resource is a singleton and a sound is one of many, so
//! [`Sound`] implements [`Asset`](pill_engine::Asset) and is addressed by
//! `Handle<Sound>`. That handle is generational, which replaces the old
//! `destroy` hook: unloading a sound leaves every [`AudioSourceComponent`] pointing at
//! it resolving to `None` on the next frame rather than needing the asset to
//! reach into every component and clear itself.
//!
//! **The audio manager is a resource, not a "global component".** This engine
//! has no global-component concept; a singleton is a resource. It holds the
//! rodio output stream, so it must never be dropped while sinks play - which
//! is exactly the lifetime a resource has.
//!
//! **Commands replace the deferred-update manager.** The old `AudioSourceComponent`
//! posted `DeferredUpdateComponentRequest`s so mutations could reach the
//! engine outside the component's own borrow. Here a system already has both
//! the component and the manager through ordinary parameters, so the intent is
//! stored as a plain [`AudioCommand`] field on the component and drained by
//! [`audio_system`]. The component stays `#[repr(C)]` plain data, which is
//! what lets it survive a hot reload and be read from C#.
//!
//! ## What is deliberately not ported
//!
//! The old `AudioSourceComponent` carried `EntityHandle`, `SceneHandle` and a
//! pointer to the deferred-update manager. None exist here: a component does
//! not know its entity, there are no scenes, and the pointer was the mechanism
//! this module replaces with commands. The builder types are gone too -
//! construction is a struct literal over public fields, as every other
//! component in this workspace is built.
//!
//! ## Spatial audio and the 2D world
//!
//! `pill_engine::common_components::Position` is 2D, and rodio's spatial sink
//! wants a 3D point. A [`Position`] is lifted to `(x, y, 0.0)`, and an
//! [`AudioListenerComponent`] carries its own `facing` angle rather than reading a
//! rotation component, because this engine has none. That keeps the module
//! self-contained; when a 3D transform arrives, only [`ear_positions`] changes.

//! ## Layout
//!
//! One file per type, which is what the engine's other multi-file crates do:
//!
//! | File | Holds |
//! |---|---|
//! | [`sound`] | the [`Sound`] asset and its loading |
//! | [`sound_type`] | [`SoundType`], the ambient/spatial discriminant |
//! | [`audio_command`] | [`AudioCommand`], the pending-intent vocabulary |
//! | [`audio_listener_component`] | the [`AudioListenerComponent`] component |
//! | [`audio_source_component`] | the [`AudioSourceComponent`] component |
//! | [`audio_manager`] | the [`AudioManager`] resource and its sink pools |
//! | [`listener_geometry`] | ear placement, testable without a device |
//! | [`audio_system`] | the per-frame system |
//!
//! Everything re-exports at the crate root, so a caller writes
//! `pill_audio::AudioSourceComponent` rather than naming the module that declares it.

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
/// then does nothing. That is what keeps this module loadable on a CI runner.
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

    /// An audio source's sound is a typed asset reference (W-0048): it reads
    /// naming the shared sound type - which is what the Inspector draws as an
    /// asset row and the picker lists - and a handle writes back into the
    /// component the playback system reads.
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
