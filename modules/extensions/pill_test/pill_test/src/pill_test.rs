//! Minimal extension module: one persistable component and one system.
//!
//! # Responsibilities
//!
//! - Defines the `ModuleTest` component and the `module_test_processor` system.
//! - Registers both through the extension ABI when the host loads it.
//! - Keeps its entities filled to a target count across hot reloads.
//!
//! # Design
//!
//! The crate is a plain `rlib`: the generated `host_module_pill_test` wrapper
//! links it into the `cdylib` the host stages and loads at runtime, next to and
//! independently of the project module. All registration work lives in
//! [`register`], which is a plain Rust function, so the same crate can also be
//! linked statically into a monolithic build. The `extern "C"` exports the
//! `#[pill_module]` attribute emits are thin wrappers that catch panics and
//! translate them into a status code, because unwinding across the module
//! boundary would abort the process.
//!
//! The component is registered as persistable, so its values survive a reload:
//! snapshots are keyed by type name, an added field takes its default and a
//! removed one is dropped.

// External crates
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
const MODULE_TEST_ENTITY_COUNT: usize = 4;

/// Frame interval a progress report would use.
///
/// No report is emitted, so nothing reads the constant and the `allow` keeps
/// the dead-code lint quiet.
#[allow(dead_code)]
const REPORT_INTERVAL_FRAMES: u64 = 300;

/// Fixed time step folded into the accumulated time of every entity.
///
/// Used only by [`module_test_processor`], which [`register`] installs.
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
/// the project's systems automatically.
fn module_test_processor(mut query: Query<&mut ModuleTest>) -> Result<(), SystemError> {
    for mut state in query.iter_mut() {
        state.processed_frame_count += 1;
        state.accumulated_time += FIXED_DELTA_TIME;
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
/// Public so a statically linked build can call it directly, and so the
/// generated `host_module_pill_test` wrapper can carry it as
/// `pill_module_init` for the host to find in a loaded DLL.
#[pill_module]
pub fn register(engine: &mut Engine) -> u32 {
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
