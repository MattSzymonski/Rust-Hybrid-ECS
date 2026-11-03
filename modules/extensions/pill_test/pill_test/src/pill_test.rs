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
//! The crate is a plain `rlib`. The generated `host_module_pill_test` wrapper
//! links it into the `cdylib` that the host stages and loads at runtime, beside
//! the project module and reloaded on its own. All registration work lives in
//! [`register`], a plain Rust function, so the same crate can also be linked
//! statically into a monolithic build. The `extern "C"` exports that
//! `#[pill_module]` emits wrap [`register`]: they catch a panic and turn it
//! into a status code, because a panic that unwound across the module boundary
//! would abort the process.
//!
//! The component is registered as persistable, so its values survive a reload.
//! The reload matches persisted types by name, gives a newly added field its
//! default, and ignores a field the type no longer declares.

// External crates
use pill_core::info;
use pill_engine::*;
use serde::{Deserialize, Serialize};

// =============================================================================
// Constants
// =============================================================================

/// Number of entities the module keeps alive.
///
/// Hot reload keeps the entities the previous generation created, so
/// initialization fills the world up to this count instead of spawning a
/// fresh batch each time.
const MODULE_TEST_ENTITY_COUNT: usize = 4;

/// Frame interval a progress report would use, counted in frames.
///
/// Nothing emits a report, so nothing reads the constant and the `allow`
/// above keeps the dead-code lint quiet.
#[allow(dead_code)]
const REPORT_INTERVAL_FRAMES: u64 = 300;

/// Fixed time step, in seconds, added to every entity's accumulated time.
///
/// Only [`module_test_processor`] uses it, and [`register`] installs that
/// system.
const FIXED_DELTA_TIME: f32 = 1.0 / 60.0;

// =============================================================================
// Component
// =============================================================================

/// Per-entity state this module defines.
///
/// The host carries the component's values across hot-reload generations, so
/// the layout is pinned with `#[repr(C)]` and every field stays serde
/// compatible. Adding or removing a field is safe: serde matches fields by
/// name, a new field gets its default, and a removed field is ignored.
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
/// Registered under the name `module_test_processor`. The scheduler derives
/// the system's access pattern from the query signature, so it places the
/// system among the project's systems automatically.
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

/// Registers the module's system and its seed entities, and returns 0 on
/// success.
///
/// Component registration happens automatically from the `PillComponent`
/// derive before this runs. Must be idempotent: the host calls it once per
/// loaded generation, on a fresh load and again if it has to roll back. Public
/// so a statically linked build can call it directly, and so the generated
/// `host_module_pill_test` wrapper can export it as `pill_module_init` for the
/// host to find in a loaded DLL.
///
/// # Errors
///
/// Returns 1 when an entity cannot be created. The host reads any non-zero
/// status as a failed generation and rolls back to the previous library, which
/// calls this function again on the older generation.
#[pill_module]
pub fn register(engine: &mut Engine) -> u32 {
    // Step 1: Register the system that advances the component.
    engine.register_system("module_test_processor", module_test_processor);

    // Step 2: Fill the world up to the target count instead of spawning a
    // fresh batch, because hot reload keeps the entities the previous
    // generation created.
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
