//! Hot-reloadable game module for migration integration testing.
//!
//! # Responsibilities
//!
//! - Defines three persistable components used by migration tests.
//! - Implements a single `counter_system` that prints a timestamp at threshold.
//! - Exports `game_init` and `game_update` for the standalone host.
//!
//! # Design
//!
//! This crate is compiled as a `cdylib` (dynamic library). The standalone
//! host loads it at runtime and calls `game_init` to register the component
//! and system. When source files change, the host rebuilds and reloads this
//! module without restarting. Component data is preserved across reloads
//! via JSON serialization and matched by type name.

// External crates
use serde::{Deserialize, Serialize};
use trait_type_map::impl_trait_accessible;

// Current crate
use pill_engine::*;

// =============================================================================
// Components
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct FrameCounter {
    count: u64,
}
impl Component for FrameCounter {}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SpatialPosition {
    horizontal: f32,
    vertical: f32,
}
impl Component for SpatialPosition {}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct LinearVelocity {
    horizontal_speed: f32,
    vertical_speed: f32,
}
impl Component for LinearVelocity {}

impl_trait_accessible!(
    dyn Component;
    FrameCounter,
    SpatialPosition,
    LinearVelocity
);

// =============================================================================
// Systems
// =============================================================================

/// Increments the counter every frame. When it reaches the threshold,
/// resets and prints a timestamp to the console.
fn counter_system(mut query: Query<&mut FrameCounter>) {
    const THRESHOLD: u64 = 200;

    for mut counter in query.iter_mut() {
        counter.count += 1;
        if counter.count >= THRESHOLD {
            counter.count = 0;

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let seconds = now.as_secs();
            let milliseconds = now.subsec_millis();

            let hours = (seconds / 3600) % 24;
            let minutes = (seconds / 60) % 60;
            let secs = seconds % 60;

            println!(
                "counter tick [{:02}:{:02}:{:02}.{:03}]",
                hours, minutes, secs, milliseconds
            );
        }
    }
}

// =============================================================================
// FFI Entry Points
// =============================================================================

/// Registers test components, systems, and seed entities; returns zero.
#[no_mangle]
pub extern "C" fn game_init(api: *const EngineApi) -> u32 {
    let api = unsafe { &*api };
    let engine: &mut Engine = unsafe { &mut *(api.engine_handle as *mut Engine) };

    engine
        .world_mut()
        .register_persistable_component::<FrameCounter>();
    engine
        .world_mut()
        .register_persistable_component::<SpatialPosition>();
    engine
        .world_mut()
        .register_persistable_component::<LinearVelocity>();

    engine.register_system("counter", counter_system);

    // Seed multiple archetypes so migration tests can validate per-component behavior.
    let _ = engine
        .world_mut()
        .create_entity()
        .with(FrameCounter { count: 0 })
        .build();

    let _ = engine
        .world_mut()
        .create_entity()
        .with(FrameCounter { count: 90 })
        .with(SpatialPosition {
            horizontal: 10.0,
            vertical: 20.0,
        })
        .build();

    let _ = engine
        .world_mut()
        .create_entity()
        .with(SpatialPosition {
            horizontal: 1.0,
            vertical: 2.0,
        })
        .with(LinearVelocity {
            horizontal_speed: 1.5,
            vertical_speed: 0.25,
        })
        .build();

    let _ = engine
        .world_mut()
        .create_entity()
        .with(FrameCounter { count: 180 })
        .with(SpatialPosition {
            horizontal: -5.0,
            vertical: 8.0,
        })
        .with(LinearVelocity {
            horizontal_speed: 0.5,
            vertical_speed: 0.75,
        })
        .build();

    // Report successful registration so the host keeps this generation.
    0
}

#[no_mangle]
pub extern "C" fn game_update(api: *const EngineApi) {
    let _ = api;
}

/// Returns a hash of persistable component TypeIds and sizes.
#[no_mangle]
pub extern "C" fn game_schema_fingerprint() -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();

    // --- Persistable components: hash TypeId + size ---
    std::any::TypeId::of::<FrameCounter>().hash(&mut hasher);
    std::mem::size_of::<FrameCounter>().hash(&mut hasher);
    std::any::TypeId::of::<SpatialPosition>().hash(&mut hasher);
    std::mem::size_of::<SpatialPosition>().hash(&mut hasher);
    std::any::TypeId::of::<LinearVelocity>().hash(&mut hasher);
    std::mem::size_of::<LinearVelocity>().hash(&mut hasher);

    hasher.finish()
}
