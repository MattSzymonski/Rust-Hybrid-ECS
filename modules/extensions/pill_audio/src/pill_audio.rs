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
/// Shared project-to-extension sound loading requests.
pub mod audio_load_queue;
/// The [`AudioListenerComponent`] component.
pub mod audio_listener_component;
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
pub use audio_load_queue::AudioLoadQueue;
pub use audio_listener_component::AudioListenerComponent;
pub use audio_manager::{
    AudioManager, DEFAULT_AMBIENT_SINK_COUNT, DEFAULT_SPATIAL_SINK_COUNT, EAR_SEPARATION,
};
pub use audio_source_component::{AudioSourceComponent, NO_SINK};
pub use audio_system::audio_system;
pub use listener_geometry::ear_positions;
pub use sound::{Sound, SoundLoadError, SUPPORTED_AUDIO_FORMATS};
pub use sound_type::SoundType;

// =============================================================================
// Registration
// =============================================================================

/// Registers this module's components, resource and system with the engine.
///
/// Called directly by a monolithic build. With `module-abi` on, `#[pill_module]`
/// also exports it as `pill_module_init` for the host to find in a loaded DLL.
///
/// Installing the [`AudioManager`] is conditional: a machine with no audio
/// device gets the components and the system but no manager, and the system
/// then does nothing. That is what keeps this module loadable on a CI runner.
#[pill_module]
pub fn register(engine: &mut Engine) -> u32 {
    if engine.world().get_resource::<AudioLoadQueue>().is_none() {
        engine.world_mut().insert_resource(AudioLoadQueue::default());
    }
    engine
        .world_mut()
        .register_component::<AudioListenerComponent>();
    engine
        .world_mut()
        .register_component::<AudioSourceComponent>();

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
