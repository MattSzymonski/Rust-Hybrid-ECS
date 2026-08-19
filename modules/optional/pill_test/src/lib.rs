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

// Standard library
use std::ffi::c_char;
use std::panic::{catch_unwind, AssertUnwindSafe};

// External crates
use pill_core::info;
use pill_engine::*;
use serde::{Deserialize, Serialize};
use trait_type_map::impl_trait_accessible;

// =============================================================================
// Constants
// =============================================================================

/// Optional-module ABI revision this crate was built against.
///
/// The host refuses to load a module whose revision it does not recognise,
/// before handing it any pointer into engine memory.
const MODULE_ABI_VERSION: u32 = 1;

/// Number of entities the module keeps alive.
///
/// Hot reload preserves entities, so initialization fills the world up to this
/// count instead of spawning a fresh batch on every rebuild.
const MODULE_TEST_ENTITY_COUNT: usize = 4;

/// Frame interval at which the module reports progress to the host log.
const REPORT_INTERVAL_FRAMES: u64 = 300;

/// Fixed time step folded into the accumulated time of every entity.
const FIXED_DELTA_TIME: f32 = 1.0 / 60.0;

/// Name reported to the host for diagnostics; null terminated for the C ABI.
const MODULE_NAME: &[u8] = b"pill_test\0";

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
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ModuleTest {
    /// Number of frames this entity has been processed for.
    pub processed_frame_count: u64,
    /// Accumulated simulated time, in seconds.
    pub accumulated_time: f32,
}

impl Component for ModuleTest {}
impl_trait_accessible!(dyn Component; ModuleTest);

// =============================================================================
// Systems
// =============================================================================

/// Advances every `ModuleTest` component by one frame.
///
/// Registered under the name `module_test_processor`; the scheduler derives the
/// system's access pattern from the query signature, so it is scheduled against
/// the project's systems automatically. Progress is reported on one entity only,
/// at a fixed frame interval, to keep the host console readable.
fn module_test_processor(mut query: Query<&mut ModuleTest>) -> Result<(), SystemError> {
    for (index, mut state) in query.iter_mut().enumerate() {
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

/// Registers the module's components and systems against the host engine.
///
/// Returns zero on success. Must be idempotent: the host calls it once per
/// loaded generation and rolls back to the previous library when it reports a
/// non-zero status, which re-runs this function on the older generation.
pub fn register(engine: &mut Engine) -> u32 {
    // Persistable registration is what makes the component survive a reload:
    // the host matches schemas by type name and migrates only changed layouts.
    engine
        .world_mut()
        .register_persistable_component::<ModuleTest>();
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

// =============================================================================
// Optional-module ABI exports
// =============================================================================

/// Module ABI revision, checked by the host before anything else is called.
#[no_mangle]
pub extern "C" fn pill_module_abi_version() -> u32 {
    MODULE_ABI_VERSION
}

/// Human-readable module name used in host log messages.
#[no_mangle]
pub extern "C" fn pill_module_name() -> *const c_char {
    MODULE_NAME.as_ptr() as *const c_char
}

/// Registers the module against the host engine; returns zero on success.
///
/// # Safety
///
/// `api` must be a valid [`EngineApi`] pointer owned by the host and kept alive
/// for the whole duration of this call.
#[no_mangle]
pub unsafe extern "C" fn pill_module_init(api: *const EngineApi) -> u32 {
    // A panic must never unwind across the C ABI boundary, so it is converted
    // into a non-zero status and the host keeps the previous generation.
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: The host guarantees `api` points at a live `EngineApi` whose
        // `engine_handle` addresses the single engine instance, and that both
        // outlive this call. The engine is not otherwise borrowed while a
        // module initializes, so the reconstructed `&mut Engine` is unique.
        let api = unsafe { &*api };
        let engine = unsafe { &mut *(api.engine_handle as *mut Engine) };
        register(engine)
    }));
    result.unwrap_or(u32::MAX)
}
