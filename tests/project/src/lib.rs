//! Hot-reloadable project module for migration integration testing.
//!
//! # Responsibilities
//!
//! - Defines three persistable components used by migration tests.
//! - Implements a single `counter_system` that prints a timestamp at threshold.
//! - Exports `project_init` and `project_update` for the standalone host.
//!
//! # Design
//!
//! This crate is compiled as a `cdylib` (dynamic library). The standalone
//! host loads it at runtime and calls `project_init` to register the component
//! and system. When source files change, the host rebuilds and reloads this
//! module without restarting. Component data is preserved across reloads
//! via JSON serialization and matched by type name.
//!
//! An engine reload additionally swaps the whole engine binary underneath the
//! project. [`ReloadWitness`] is registered as a persistable resource so the
//! integration suite can prove that singleton state, not just component
//! columns, survives that swap: it counts how many times `project_init` has
//! run and is printed once per second by [`reload_witness_system`].

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
// Resources
// =============================================================================

/// Singleton state proving persistable resources survive an engine swap.
///
/// `initialization_count` is incremented by every `project_init` call, so it
/// grows on a project reload. `preserved_marker` is only ever written here on
/// the very first initialization: after an engine swap the restored value must
/// come back unchanged, which is what the integration suite asserts.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ReloadWitness {
    initialization_count: u64,
    preserved_marker: u64,
    reported_at_tick: u64,
}
impl Resource for ReloadWitness {}

/// Marker value written once and expected back after every reload.
const PRESERVED_MARKER: u64 = 0xC0FFEE;

/// How many frames pass between two witness reports.
///
/// At the capped 60 frames per second this project runs at, the witness
/// reports roughly every two seconds: often enough for an integration suite to
/// observe one shortly after a reload, rare enough not to drown the log.
const WITNESS_REPORT_INTERVAL: u64 = 120;

// =============================================================================
// Systems
// =============================================================================

/// Print the persisted witness resource at a low, steady frequency.
///
/// The suite matches this line to confirm that a resource survived a reload
/// with its marker and initialization count intact.
fn reload_witness_system(mut witness: ResMut<ReloadWitness>) -> Result<(), SystemError> {
    let Some(mut witness) = witness.get_mut() else {
        return Err(SystemError::MissingResource {
            name: String::from("ReloadWitness"),
        });
    };

    witness.reported_at_tick += 1;
    if witness.reported_at_tick % WITNESS_REPORT_INTERVAL != 0 {
        return Ok(());
    }
    println!(
        "reload witness marker={:#X} initializations={}",
        witness.preserved_marker, witness.initialization_count,
    );
    Ok(())
}

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
///
/// # Safety
///
/// `api` must be a valid [`EngineApi`] pointer owned by the host for the
/// complete duration of this call.
#[no_mangle]
pub unsafe extern "C" fn project_init(api: *const EngineApi) -> u32 {
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

    // The witness resource is registered before it is inserted so a restore
    // performed after this call can find its deserializer by type name.
    engine
        .world_mut()
        .register_persistable_resource::<ReloadWitness>();

    // Only the first initialization writes the marker. Every later one bumps
    // the counter, so a restored resource is distinguishable from a fresh one.
    let existing_witness = engine.world().get_resource::<ReloadWitness>().cloned();
    let witness = match existing_witness {
        Some(mut witness) => {
            witness.initialization_count += 1;
            witness
        }
        None => ReloadWitness {
            initialization_count: 1,
            preserved_marker: PRESERVED_MARKER,
            reported_at_tick: 0,
        },
    };
    engine.world_mut().insert_resource(witness);

    // Cap the frame rate so the counter and witness systems report at a rate a
    // log-scraping integration suite can follow. Without a cap this headless
    // project runs at hundreds of thousands of frames per second and floods the
    // host's output faster than any reload trace could be read out of it.
    engine.set_fps_limit(60.0);

    engine.register_system("counter", counter_system);
    engine.register_system("reload_witness", reload_witness_system);

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
pub extern "C" fn project_update(api: *const EngineApi) {
    let _ = api;
}

/// Returns a hash of persistable component TypeIds and sizes.
#[no_mangle]
pub extern "C" fn project_schema_fingerprint() -> u64 {
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
