//! Hot-reloadable game module — minimal single-component counter for testing.
//!
//! # Responsibilities
//!
//! - Defines a single `FrameCounter` component.
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
use ecs_hybrid::*;

// =============================================================================
// Components
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct FrameCounter {
    count: u64,
}
impl Component for FrameCounter {}

impl_trait_accessible!(dyn Component; FrameCounter);

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

#[no_mangle]
pub extern "C" fn game_init(api: *const EngineApi) {
    let api = unsafe { &*api };
    let engine: &mut Engine = unsafe { &mut *(api.engine_handle as *mut Engine) };

    engine
        .world_mut()
        .register_persistable_component::<FrameCounter>();
    engine.register_system("counter", counter_system);

    // Spawn one entity to drive the counter.
    let _ = engine
        .world_mut()
        .create_entity()
        .with(FrameCounter { count: 0 })
        .build();
}

#[no_mangle]
pub extern "C" fn game_update(api: *const EngineApi) {
    let _ = api;
}

/// Returns a hash of all persistable component TypeIds and sizes.
///
/// The standalone host calls this *before* `game_init` on every reload.
/// If the fingerprint matches the previous DLL's fingerprint, no component
/// schema has changed — the host takes a fast path that skips
/// serialization and only swaps systems.  If it differs, the host takes
/// the slow path: snapshot all entities, migrate, restore.
///
/// When you add or change a persistable component, add its TypeId here.
#[no_mangle]
pub extern "C" fn game_schema_fingerprint() -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();

    // --- Persistable components: hash TypeId + size ---
    std::any::TypeId::of::<FrameCounter>().hash(&mut hasher);
    std::mem::size_of::<FrameCounter>().hash(&mut hasher);
    // Add new persistable components here.

    hasher.finish()
}
