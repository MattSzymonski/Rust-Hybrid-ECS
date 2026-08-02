//! Language-agnostic engine API for external hot-reloadable game consumers.
//!
//! # Responsibilities
//!
//! - Defines the [`EngineApi`] struct — a C-compatible function-pointer table
//!   passed across the FFI boundary to dynamically loaded game modules.
//! - Provides function-pointer type aliases for all API operations.
//! - Implements the actual C-callable functions that wrap [`Engine`] methods.
//! - Constructs a fully populated [`EngineApi`] via [`EngineApi::new`].
//!
//! # Design
//!
//! The standalone host owns the [`Engine`](crate::Engine). It builds an
//! [`EngineApi`] populated with function pointers that all target the same
//! engine instance. The game module receives this struct through its exported
//! `game_init` entry point.
//!
//! ## Two consumption paths
//!
//! | Consumer language | How it uses EngineApi |
//! |-------------------|----------------------|
//! | Rust              | Casts `engine_handle` to `&mut Engine` and calls the full typed API directly. Function pointers are ignored. |
//! | C / C++ / Zig     | Passes `engine_handle` to every function pointer in the table. Never touches Rust types. |
//!
//! This dual design avoids forcing a lowest-common-denominator C API on Rust
//! consumers while still enabling future non-Rust game modules.

// Standard library
use std::ffi::{c_char, c_void};

// Current crate
use crate::engine::Engine;

// =============================================================================
// Function Pointer Type Aliases
// =============================================================================

/// Registers a component type by name, size, and alignment.
///
/// Returns 0 on success, non-zero if registration fails (e.g., component
/// limit exceeded).
pub type ComponentRegisterFn = unsafe extern "C" fn(
    engine: *mut c_void,
    type_name: *const c_char,
    size_in_bytes: u32,
    alignment_in_bytes: u32,
) -> i32;

/// Registers a raw system function that receives the engine handle.
///
/// The system is called once per frame during `process_frame`. The
/// function must use the [`EngineApi`] function pointers to interact
/// with the world — it cannot access Rust types directly.
///
/// Returns 0 on success, non-zero on failure.
pub type SystemRegisterRawFn = unsafe extern "C" fn(
    engine: *mut c_void,
    system_name: *const c_char,
    system_function: unsafe extern "C" fn(*mut c_void),
) -> i32;

/// Processes one frame: executes all registered systems, then applies
/// deferred commands.
///
/// Returns 0 on success, non-zero if a fatal command error occurred
/// (only when the engine's `should_exit_on_error` flag is set).
pub type ProcessFrameFn = unsafe extern "C" fn(engine: *mut c_void) -> i32;

/// Limits the frame rate to `frames_per_second`.
///
/// Pass 0.0 or infinity to disable the limiter.
pub type SetFpsLimitFn = unsafe extern "C" fn(engine: *mut c_void, frames_per_second: f64);

/// Enables or disables parallel system execution.
pub type SetParallelExecutionFn = unsafe extern "C" fn(engine: *mut c_void, enabled: bool);

/// Returns the number of living entities in the world.
pub type EntityCountFn = unsafe extern "C" fn(engine: *mut c_void) -> u64;

/// Pre-allocates internal storage for at least `capacity` entities.
pub type ReserveEntitiesFn = unsafe extern "C" fn(engine: *mut c_void, capacity: u64);

// =============================================================================
// EngineApi
// =============================================================================

/// C-compatible function-pointer table for interacting with the ECS engine.
///
/// Rust game modules should cast `engine_handle` to `&mut Engine` and use
/// the full typed API. Non-Rust modules should call the function pointers,
/// passing `engine_handle` as the first argument.
///
/// # Safety
///
/// All function pointers and the `engine_handle` are only valid while the
/// host's [`Engine`](crate::Engine) lives and while the game module is loaded.
/// The game must not store any of these pointers beyond the duration of the
/// `game_init` / `game_update` call.
///
/// # Examples
///
/// **Rust game module** (full typed API):
///
/// ```ignore
/// #[no_mangle]
/// pub extern "C" fn game_init(api: *const EngineApi) {
///     let api = unsafe { &*api };
///     let engine: &mut Engine = unsafe { &mut *(api.engine_handle as *mut Engine) };
///     engine.register_component::<Position>();
///     engine.register_system("movement", movement_system);
/// }
/// ```
///
/// **C game module** (function pointers only):
///
/// ```c
/// void game_init(const EngineApi* api) {
///     api->register_component(api->engine_handle, "Position", 8, 4);
///     api->set_fps_limit(api->engine_handle, 60.0);
/// }
/// ```
#[repr(C)]
pub struct EngineApi {
    /// Opaque handle to the engine instance.
    ///
    /// Rust consumers cast this to `&mut Engine`. Non-Rust consumers pass
    /// it as the first argument to every function pointer in this struct.
    pub engine_handle: *mut c_void,

    // --- Component Registration ---
    /// Register a component type by name, size, and alignment.
    ///
    /// **Rust consumers**: prefer `engine.world_mut().register_component::<T>()`
    /// via the `engine_handle` cast instead — it is type-safe and automatic.
    /// This function pointer exists for non-Rust language bindings.
    pub register_component: ComponentRegisterFn,

    // --- System Registration (raw) ---
    /// Register a system from a raw C function pointer.
    ///
    /// **Rust consumers**: prefer `engine.register_system(name, rust_function)`
    /// via the `engine_handle` cast. This function pointer exists for non-Rust
    /// language bindings.
    ///
    /// The registered function receives the engine handle and must use the
    /// [`EngineApi`] function pointers for all world interaction.
    pub register_system_raw: SystemRegisterRawFn,

    // --- Frame Processing ---
    /// Execute all registered systems and apply deferred commands.
    pub process_frame: ProcessFrameFn,

    // --- Configuration ---
    /// Limit the frame rate. Pass 0.0 to disable.
    pub set_fps_limit: SetFpsLimitFn,

    /// Enable or disable parallel system execution.
    pub set_parallel_execution: SetParallelExecutionFn,

    // --- World Queries ---
    /// Number of living entities.
    pub entity_count: EntityCountFn,

    /// Pre-allocate storage for at least `capacity` entities.
    pub reserve_entities: ReserveEntitiesFn,
}

impl EngineApi {
    /// Build a fully populated [`EngineApi`] that targets the given engine.
    ///
    /// All function pointers are set to C-callable wrappers around the
    /// engine's methods. The returned struct is safe to pass across FFI.
    pub fn new(engine: &mut Engine) -> Self {
        Self {
            engine_handle: engine as *mut Engine as *mut c_void,

            register_component: api_register_component,
            register_system_raw: api_register_system_raw,
            process_frame: api_process_frame,
            set_fps_limit: api_set_fps_limit,
            set_parallel_execution: api_set_parallel_execution,
            entity_count: api_entity_count,
            reserve_entities: api_reserve_entities,
        }
    }
}

// =============================================================================
// C-Callable Wrapper Functions
// =============================================================================
//
// Each function below follows the same pattern:
//   1. Cast `engine: *mut c_void` back to `&mut Engine`.
//   2. Call the corresponding method.
//   3. Return a C-compatible result (0 = success, non-zero = error).
//
// SAFETY: All functions assume the `engine` pointer is a valid `*mut Engine`
// that was created by the host and has not been dropped. The host guarantees
// this invariant.

/// # Safety
///
/// `engine` must be a valid `*mut Engine` that outlives this call.
unsafe extern "C" fn api_register_component(
    engine: *mut c_void,
    type_name: *const c_char,
    _size_in_bytes: u32,
    _alignment_in_bytes: u32,
) -> i32 {
    // SAFETY: Caller guarantees the engine pointer is valid.
    let _engine_ref: &mut Engine = unsafe { &mut *(engine as *mut Engine) };

    // Raw component registration by name/size/alignment is not yet
    // implemented on the Engine.  Rust consumers should use the typed
    // `engine_handle` cast + `register_component::<T>()` instead.
    // Non-Rust consumers will need a future `register_component_raw` method.

    let name_str = if type_name.is_null() {
        "(null)"
    } else {
        // SAFETY: Caller guarantees a null-terminated C string.
        unsafe { std::ffi::CStr::from_ptr(type_name) }
            .to_str()
            .unwrap_or("(invalid utf-8)")
    };
    eprintln!(
        "[engine_api] register_component_raw is not yet implemented. \
         Tried to register '{}' ({} bytes, align {}). \
         Use the typed Rust API instead.",
        name_str, _size_in_bytes, _alignment_in_bytes,
    );

    let _ = _engine_ref;
    // Return error for now — raw registration needs engine-side support.
    -1
}

/// # Safety
///
/// `engine` must be a valid `*mut Engine` that outlives this call.
unsafe extern "C" fn api_register_system_raw(
    engine: *mut c_void,
    system_name: *const c_char,
    _system_function: unsafe extern "C" fn(*mut c_void),
) -> i32 {
    // SAFETY: Caller guarantees the engine pointer is valid.
    let _engine_ref: &mut Engine = unsafe { &mut *(engine as *mut Engine) };

    let name_str = if system_name.is_null() {
        "(null)"
    } else {
        // SAFETY: Caller guarantees a null-terminated C string.
        unsafe { std::ffi::CStr::from_ptr(system_name) }
            .to_str()
            .unwrap_or("(invalid utf-8)")
    };
    eprintln!(
        "[engine_api] register_system_raw is not yet implemented. \
         Tried to register system '{}'. \
         Use the typed Rust API instead.",
        name_str,
    );

    let _ = _engine_ref;
    let _ = _system_function;
    // Return error for now — raw system registration needs engine-side support.
    -1
}

/// # Safety
///
/// `engine` must be a valid `*mut Engine` that outlives this call.
unsafe extern "C" fn api_process_frame(engine: *mut c_void) -> i32 {
    // SAFETY: Caller guarantees the engine pointer is valid.
    let engine_ref: &mut Engine = unsafe { &mut *(engine as *mut Engine) };
    match engine_ref.process_frame() {
        Ok(()) => 0,
        Err(_errors) => {
            // Errors are logged internally by the engine when
            // `should_exit_on_error` is false. When true, they are
            // returned here.
            1
        }
    }
}

/// # Safety
///
/// `engine` must be a valid `*mut Engine` that outlives this call.
unsafe extern "C" fn api_set_fps_limit(engine: *mut c_void, frames_per_second: f64) {
    // SAFETY: Caller guarantees the engine pointer is valid.
    let engine_ref: &mut Engine = unsafe { &mut *(engine as *mut Engine) };
    engine_ref.set_fps_limit(frames_per_second);
}

/// # Safety
///
/// `engine` must be a valid `*mut Engine` that outlives this call.
unsafe extern "C" fn api_set_parallel_execution(engine: *mut c_void, enabled: bool) {
    // SAFETY: Caller guarantees the engine pointer is valid.
    let engine_ref: &mut Engine = unsafe { &mut *(engine as *mut Engine) };
    engine_ref.set_parallel_execution(enabled);
}

/// # Safety
///
/// `engine` must be a valid `*mut Engine` that outlives this call.
unsafe extern "C" fn api_entity_count(engine: *mut c_void) -> u64 {
    // SAFETY: Caller guarantees the engine pointer is valid.
    let engine_ref: &Engine = unsafe { &*(engine as *const Engine) };
    engine_ref.world().entity_count() as u64
}

/// # Safety
///
/// `engine` must be a valid `*mut Engine` that outlives this call.
unsafe extern "C" fn api_reserve_entities(engine: *mut c_void, capacity: u64) {
    // SAFETY: Caller guarantees the engine pointer is valid.
    let engine_ref: &mut Engine = unsafe { &mut *(engine as *mut Engine) };
    engine_ref.world_mut().reserve_entities(capacity as usize);
}
