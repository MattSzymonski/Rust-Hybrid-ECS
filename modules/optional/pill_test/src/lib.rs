//! Optional engine module used to exercise dynamic module loading.
//!
//! # Responsibilities
//!
//! - Defines the `ModuleTest` component and the `module_test_processor` system.
//! - Registers both through the optional-module ABI when the host loads it.
//! - Keeps its entities filled to a target count across hot reloads.
//!
//! # Design
//!
//! The crate is built as a `cdylib` and loaded by `pill_host` at runtime, next
//! to and independently of the project module. All registration work lives in
//! [`register`], which is a plain Rust function, so the same crate can also be
//! linked statically into a monolithic build. The `extern "C"` exports are thin
//! wrappers that catch panics and translate them into a status code, because
//! unwinding across the module boundary would abort the process.
//!
//! The component is registered as persistable, so its values survive a reload:
//! the host matches schemas by type name and migrates only changed layouts.

// External crates
#[cfg(feature = "module-abi")]
use pill_core::info;
use pill_engine::*;
use serde::{Deserialize, Serialize};

// =============================================================================
// Constants
// =============================================================================

/// Number of entities the module keeps alive.
///
/// Hot reload preserves entities, so initialization fills the world up to this
/// count instead of spawning a fresh batch on every rebuild.
#[cfg(feature = "module-abi")]
const MODULE_TEST_ENTITY_COUNT: usize = 4;

/// Frame interval at which the module reports progress to the host log.
///
/// Used only by the progress-reporting block below, which is temporarily
/// disabled; the allow goes away with it.
#[allow(dead_code)]
const REPORT_INTERVAL_FRAMES: u64 = 300;

/// Fixed time step folded into the accumulated time of every entity.
///
/// Used only by [`module_test_processor`], which the module-abi build registers.
#[cfg(feature = "module-abi")]
const FIXED_DELTA_TIME: f32 = 1.0 / 60.0;

// =============================================================================
// Component
// =============================================================================

/// Per-entity state owned by this module.
///
/// The host serializes the component across hot-reload generations, so the
/// layout is pinned with `#[repr(C)]` and every field stays serde compatible.
/// Adding or removing a field is safe: serde matches by name, new fields get
/// their default, and removed fields are dropped.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PillComponent)]
#[pill(persistable)]
pub struct ModuleTest {
    /// Number of frames this entity has been processed for.
    pub processed_frame_count: u64,
    /// Accumulated simulated time, in seconds.
    pub accumulated_time: f32,
}

// =============================================================================
// Systems
// =============================================================================

/// Advances every `ModuleTest` component by one frame.
///
/// Registered under the name `module_test_processor`; the scheduler derives the
/// system's access pattern from the query signature, so it is scheduled against
/// the project's systems automatically. Progress is reported on one entity only,
/// at a fixed frame interval, to keep the host console readable.
#[cfg(feature = "module-abi")]
fn module_test_processor(mut query: Query<&mut ModuleTest>) -> Result<(), SystemError> {
    for (_, mut state) in query.iter_mut().enumerate() {
        state.processed_frame_count += 1;
        state.accumulated_time += FIXED_DELTA_TIME;

        // if index == 0 && state.processed_frame_count % REPORT_INTERVAL_FRAMES == 0 {
        //     info!(
        //         target: pill_core::telemetry::telemetry_target::ECS,
        //         frames = state.processed_frame_count,
        //         seconds = state.accumulated_time,
        //         "pill_test module still processing"
        //     );
        // }
    }
    Ok(())
}

// =============================================================================
// Registration
// =============================================================================

/// Registers the module's systems and seed entities; component registration
/// happens automatically from the `PillComponent` derive before this runs.
///
/// Returns zero on success. Must be idempotent: the host calls it once per
/// loaded generation and rolls back to the previous library when it reports a
/// non-zero status, which re-runs this function on the older generation.
#[pill_module]
fn register(engine: &mut Engine) -> u32 {
    engine.register_system("module_test_processor", module_test_processor);

    // Fill up to the target count rather than spawning a new batch, because
    // hot reload preserves the entities created by the previous generation.
    let existing_entity_count = {
        let mut query = Query::<&ModuleTest>::new(engine.world_mut());
        query.iter_mut().count()
    };
    for _ in existing_entity_count..MODULE_TEST_ENTITY_COUNT {
        if engine
            .world_mut()
            .create_entity()
            .with(ModuleTest::default())
            .build()
            .is_err()
        {
            // Report the failure so the host keeps the previous generation
            // instead of running with a half-populated world.
            return 1;
        }
    }

    info!(
        target: pill_core::telemetry::telemetry_target::ECS,
        entities = MODULE_TEST_ENTITY_COUNT,
        existing = existing_entity_count,
        "pill_test module registered"
    );
    0
}
