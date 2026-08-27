//! Language-agnostic engine API for external hot-reloadable project consumers.
//!
//! # Responsibilities
//!
//! - Defines the [`EngineApi`] struct — a C-compatible function-pointer table
//!   passed across the FFI boundary to dynamically loaded project modules.
//! - Provides function-pointer type aliases for all API operations.
//! - Implements the actual C-callable functions that wrap [`Engine`] methods.
//! - Constructs a fully populated [`EngineApi`] via [`EngineApi::new`].
//!
//! # Design
//!
//! The standalone host owns the [`Engine`](crate::Engine). It builds an
//! [`EngineApi`] populated with function pointers that all target the same
//! engine instance. The project module receives this struct through its exported
//! `project_init` entry point.
//!
//! ## Two consumption paths
//!
//! | Consumer language | How it uses EngineApi |
//! |-------------------|----------------------|
//! | Rust              | Casts `engine_handle` to `&mut Engine` and calls the full typed API directly. Function pointers are ignored. |
//! | C / C++ / Zig     | Passes `engine_handle` to every function pointer in the table. Never touches Rust types. |
//!
//! This dual design avoids forcing a lowest-common-denominator C API on Rust
//! consumers while still enabling future non-Rust project modules.

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
///
/// Returns 0 on success, non-zero when the request is rejected: a null handle,
/// a capacity that cannot be represented on this target, or one exceeding
/// [`MAX_ENTITIES_PER_RESERVE`].
pub type ReserveEntitiesFn = unsafe extern "C" fn(engine: *mut c_void, capacity: u64) -> i32;

/// Largest `reserve_entities` request the ABI accepts.
///
/// The request flows straight into a `Vec::reserve` on the free list, so an
/// unvalidated value could ask for an allocation that aborts the process.
/// The bound is generous (a game would need hundreds of gigabytes of entity
/// bookkeeping to need more) but keeps the failure reportable instead of
/// fatal.
pub const MAX_ENTITIES_PER_RESERVE: u64 = 1 << 24;

// =============================================================================
// EngineApi
// =============================================================================

/// C-compatible function-pointer table for interacting with the ECS engine.
///
/// Rust project modules should cast `engine_handle` to `&mut Engine` and use
/// the full typed API. Non-Rust modules should call the function pointers,
/// passing `engine_handle` as the first argument.
///
/// # Safety
///
/// All function pointers and the `engine_handle` are only valid while the
/// host's [`Engine`](crate::Engine) lives and while the project module is loaded.
/// The project must not store any of these pointers beyond the duration of the
/// `project_init` / `project_update` call.
///
/// # Examples
///
/// **Rust project module** (full typed API):
///
/// ```ignore
/// #[no_mangle]
/// pub extern "C" fn project_init(api: *const EngineApi) {
///     let api = unsafe { &*api };
///     let engine: &mut Engine = unsafe { &mut *(api.engine_handle as *mut Engine) };
///     engine.register_component::<Position>();
///     engine.register_system("movement", movement_system);
/// }
/// ```
///
/// **C project module** (function pointers only):
///
/// ```c
/// void project_init(const EngineApi* api) {
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
        // Capture the engine as an opaque handle and wire every function
        // pointer to the C-callable wrapper for that same engine.
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

// =============================================================================
// Guarded Entry
// =============================================================================

/// Run one C ABI body with the two protections every foreign entry point needs.
///
/// Both are required for soundness rather than politeness:
///
/// - **Null rejection.** Forming `&mut *ptr` from a null pointer is undefined
///   behaviour in Rust even when the reference is never read, so the check has
///   to happen before the reference exists, not before it is used.
/// - **Unwind containment.** Letting a panic cross an `extern "C"` boundary is
///   undefined behaviour. The engine cannot use `panic = "abort"` (`pill_core`
///   is a `dylib`, and rustc rejects that pairing), so catching here is the
///   only mechanism available. The generated `#[pill_project]` and
///   `#[pill_module]` wrappers already do exactly this; this brings the
///   engine's own ABI to the same standard.
///
/// `on_failure` is returned for a null handle and for a caught panic, so each
/// wrapper reports failure in whatever vocabulary its signature uses.
///
/// # Safety
///
/// `engine` must be null or a valid `*mut Engine` that outlives this call, and
/// must not be aliased for the duration of `body`.
unsafe fn with_engine<R>(
    engine: *mut c_void,
    on_failure: R,
    body: impl FnOnce(&mut Engine) -> R,
) -> R {
    // `as_mut` performs the null check and only then forms the reference, which
    // is the ordering the UB rule requires.
    //
    // SAFETY: the caller guarantees a valid, unaliased `Engine` pointer when it
    // is non-null; `as_mut` rejects the null case before any reference exists.
    let Some(engine_ref) = (unsafe { (engine as *mut Engine).as_mut() }) else {
        return on_failure;
    };
    // `AssertUnwindSafe` is justified because a caught panic here is terminal
    // for the call: the failure value is returned and the engine is not used
    // again by this wrapper, so no observer sees a torn intermediate state.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(engine_ref)))
        .unwrap_or(on_failure)
}

/// C-callable wrapper for raw component registration.
///
/// Reports an error for now: the engine does not yet implement
/// name/size/alignment-based registration.
///
/// # Safety
///
/// The engine handle is unused: this entry point is unimplemented and never
/// forms a reference from it, so any value including null is safe to pass.
/// `type_name` must be null or a NUL-terminated string readable for this call.
unsafe extern "C" fn api_register_component(
    _engine: *mut c_void,
    type_name: *const c_char,
    _size_in_bytes: u32,
    _alignment_in_bytes: u32,
) -> i32 {
    // Step 2: Decode the component name for the diagnostic message.
    //
    // Raw component registration by name/size/alignment is not yet
    // implemented on the Engine.  Rust consumers should use the typed
    // `engine_handle` cast + `register_component::<T>()` instead.
    // Non-Rust consumers will need a future `register_component_raw` method.

    let name_str = if type_name.is_null() {
        "(null)"
    } else {
        // SAFETY: The caller promises `type_name` points to a
        // null-terminated C string that remains valid for reads for the
        // whole duration of this call, satisfying `CStr::from_ptr`'s
        // validity contract.
        unsafe { std::ffi::CStr::from_ptr(type_name) }
            .to_str()
            .unwrap_or("(invalid utf-8)")
    };
    // Step 3: Report the unimplemented operation and fail the call.
    eprintln!(
        "[engine_api] register_component_raw is not yet implemented. \
         Tried to register '{}' ({} bytes, align {}). \
         Use the typed Rust API instead.",
        name_str, _size_in_bytes, _alignment_in_bytes,
    );

    // Return error for now — raw registration needs engine-side support.
    -1
}

/// C-callable wrapper for raw system registration.
///
/// Reports an error for now: the engine does not yet implement
/// raw-function-pointer system registration.
///
/// # Safety
///
/// The engine handle is unused: this entry point is unimplemented and never
/// forms a reference from it, so any value including null is safe to pass.
/// `system_name` must be null or a NUL-terminated string readable for this call.
unsafe extern "C" fn api_register_system_raw(
    _engine: *mut c_void,
    system_name: *const c_char,
    _system_function: unsafe extern "C" fn(*mut c_void),
) -> i32 {
    // Step 2: Decode the system name for the diagnostic message.
    let name_str = if system_name.is_null() {
        "(null)"
    } else {
        // SAFETY: The caller promises `system_name` points to a
        // null-terminated C string that remains valid for reads for the
        // whole duration of this call, satisfying `CStr::from_ptr`'s
        // validity contract.
        unsafe { std::ffi::CStr::from_ptr(system_name) }
            .to_str()
            .unwrap_or("(invalid utf-8)")
    };

    // Step 3: Report the unimplemented operation and fail the call.
    eprintln!(
        "[engine_api] register_system_raw is not yet implemented. \
         Tried to register system '{}'. \
         Use the typed Rust API instead.",
        name_str,
    );

    let _ = _system_function;
    // Return error for now — raw system registration needs engine-side support.
    -1
}

/// C-callable wrapper around `Engine::process_frame`.
///
/// Returns 0 on success and non-zero when a fatal command error occurs and
/// the engine's `should_exit_on_error` flag is set.
///
/// # Safety
///
/// `engine` must be a valid `*mut Engine` that outlives this call.
unsafe extern "C" fn api_process_frame(engine: *mut c_void) -> i32 {
    // SAFETY: The handle was created by `EngineApi::new` via
    // `engine as *mut Engine as *mut c_void`, so it points to a valid,
    // correctly aligned `Engine`. The host keeps the engine alive for the
    // entire time the project module is loaded, so the engine outlives this
    // call. No other reference to the engine exists during this call, so
    // the reconstructed `&mut Engine` is the only outstanding reference
    // (no overlapping `&mut` aliasing).
    // SAFETY: see `with_engine` - it rejects null before forming a reference
    // and contains any unwind, which is what this ABI boundary requires.
    unsafe {
        with_engine(engine, -1, |engine| match engine.process_frame() {
            Ok(()) => 0,
            Err(_errors) => {
                // Errors are logged internally by the engine when
                // `should_exit_on_error` is false. When true, they are
                // returned here.
                1
            }
        })
    }
}

/// C-callable wrapper around `Engine::set_fps_limit`.
///
/// # Safety
///
/// `engine` must be a valid `*mut Engine` that outlives this call.
unsafe extern "C" fn api_set_fps_limit(engine: *mut c_void, frames_per_second: f64) {
    // SAFETY: The handle was created by `EngineApi::new` via
    // `engine as *mut Engine as *mut c_void`, so it points to a valid,
    // correctly aligned `Engine`. The host keeps the engine alive for the
    // entire time the project module is loaded, so the engine outlives this
    // call. No other reference to the engine exists during this call, so
    // the reconstructed `&mut Engine` is the only outstanding reference
    // (no overlapping `&mut` aliasing).
    // SAFETY: see `with_engine`.
    unsafe {
        with_engine(engine, (), |engine| engine.set_fps_limit(frames_per_second));
    }
}

/// C-callable wrapper around `Engine::set_parallel_execution`.
///
/// # Safety
///
/// `engine` must be a valid `*mut Engine` that outlives this call.
unsafe extern "C" fn api_set_parallel_execution(engine: *mut c_void, enabled: bool) {
    // SAFETY: The handle was created by `EngineApi::new` via
    // `engine as *mut Engine as *mut c_void`, so it points to a valid,
    // correctly aligned `Engine`. The host keeps the engine alive for the
    // entire time the project module is loaded, so the engine outlives this
    // call. No other reference to the engine exists during this call, so
    // the reconstructed `&mut Engine` is the only outstanding reference
    // (no overlapping `&mut` aliasing).
    // SAFETY: see `with_engine`.
    unsafe {
        with_engine(engine, (), |engine| engine.set_parallel_execution(enabled));
    }
}

/// C-callable wrapper around `Engine::world().entity_count()`.
///
/// # Safety
///
/// `engine` must be a valid `*mut Engine` that outlives this call.
unsafe extern "C" fn api_entity_count(engine: *mut c_void) -> u64 {
    // SAFETY: The handle was created by `EngineApi::new` via
    // `engine as *mut Engine as *mut c_void`, so it points to a valid,
    // correctly aligned `Engine`. The host keeps the engine alive for the
    // entire time the project module is loaded, so the engine outlives this
    // call. Only a shared `&Engine` is reconstructed here, so no mutable
    // aliasing is introduced; the caller must not mutate the engine
    // concurrently with this call.
    // SAFETY: see `with_engine`. Zero is the failure value: a caller that
    // passed a null handle has no entities to count.
    unsafe { with_engine(engine, 0, |engine| engine.world().entity_count() as u64) }
}

/// C-callable wrapper around `Engine::world_mut().reserve_entities`.
///
/// Returns 0 on success and -1 when the request is rejected: a null handle, a
/// capacity that cannot be represented as a `usize` on this target, or one
/// exceeding [`MAX_ENTITIES_PER_RESERVE`].
///
/// # Safety
///
/// `engine` must be a valid `*mut Engine` that outlives this call.
unsafe extern "C" fn api_reserve_entities(engine: *mut c_void, capacity: u64) -> i32 {
    // SAFETY: The handle was created by `EngineApi::new` via
    // `engine as *mut Engine as *mut c_void`, so it points to a valid,
    // correctly aligned `Engine`. The host keeps the engine alive for the
    // entire time the project module is loaded, so the engine outlives this
    // call. No other reference to the engine exists during this call, so
    // the reconstructed `&mut Engine` is the only outstanding reference
    // (no overlapping `&mut` aliasing).
    // A `u64` request cannot always be represented as a `usize`, and truncating
    // it silently would reserve a wildly different amount than was asked for.
    // The request is also bounded so a hostile or buggy value cannot drive the
    // free-list allocation to an abort.
    let Ok(capacity) = usize::try_from(capacity) else {
        return -1;
    };
    if capacity as u64 > MAX_ENTITIES_PER_RESERVE {
        return -1;
    }
    // SAFETY: see `with_engine`.
    unsafe {
        with_engine(engine, -1, |engine| {
            engine.world_mut().reserve_entities(capacity);
            0
        })
    }
}

// =============================================================================
// Tests
// =============================================================================

/// The C ABI is called by bindings outside Rust's guarantees, so it has to
/// survive the inputs those bindings actually produce.
#[cfg(test)]
mod abi_guard_tests {
    use super::*;

    /// A null handle must be rejected, never dereferenced.
    ///
    /// Forming `&mut *ptr` from null is undefined behaviour even when the
    /// reference is unused, so this asserts on the documented failure value
    /// rather than on "it did not crash".
    #[test]
    fn every_entry_point_rejects_a_null_handle() {
        let null = std::ptr::null_mut();

        // SAFETY: passing null is exactly the contract under test; the wrappers
        // are required to reject it before forming a reference.
        unsafe {
            assert_eq!(
                api_process_frame(null),
                -1,
                "process_frame must report failure"
            );
            assert_eq!(api_entity_count(null), 0, "entity_count must report zero");
            // The void-returning wrappers must simply not fault.
            api_set_fps_limit(null, 60.0);
            api_set_parallel_execution(null, true);
            assert_eq!(
                api_reserve_entities(null, 128),
                -1,
                "reserve_entities must report failure on a null handle"
            );
        }
    }

    /// A capacity that cannot be represented as a `usize` must be refused
    /// rather than truncated into a much smaller reservation.
    #[test]
    fn an_unrepresentable_capacity_is_refused() {
        let mut engine = Engine::new();
        let before = engine.world().entity_count();
        let handle = &mut engine as *mut Engine as *mut c_void;

        // SAFETY: `handle` addresses a live engine for the duration of the call.
        let status = unsafe { api_reserve_entities(handle, u64::MAX) };
        assert_eq!(status, -1, "an unrepresentable request must be refused");

        assert_eq!(
            engine.world().entity_count(),
            before,
            "an unrepresentable request must change nothing"
        );
    }

    /// A request beyond the documented bound is refused rather than allowed
    /// to drive the free-list allocation into an abort.
    #[test]
    fn an_over_budget_capacity_is_refused() {
        let mut engine = Engine::new();
        let handle = &mut engine as *mut Engine as *mut c_void;

        // SAFETY: `handle` addresses a live engine for the duration of the call.
        let status = unsafe { api_reserve_entities(handle, MAX_ENTITIES_PER_RESERVE + 1) };
        assert_eq!(
            status, -1,
            "a request beyond MAX_ENTITIES_PER_RESERVE must be refused"
        );
    }

    /// A valid handle still works - the guard must not break the happy path.
    #[test]
    fn a_valid_handle_is_still_serviced() {
        let mut engine = Engine::new();
        let handle = &mut engine as *mut Engine as *mut c_void;

        // SAFETY: `handle` addresses a live engine for the duration of the call.
        let count = unsafe { api_entity_count(handle) };
        assert_eq!(count, 0, "a fresh world has no entities");

        // SAFETY: as above.
        unsafe {
            api_set_parallel_execution(handle, false);
            assert_eq!(
                api_reserve_entities(handle, 16),
                0,
                "a bounded reserve must succeed"
            );
        }
    }

    /// A panic inside a system must not unwind across the ABI boundary.
    ///
    /// `catch_unwind` converts it to the wrapper's failure value; without that
    /// the unwind would be undefined behaviour at the `extern "C"` frame.
    #[test]
    fn a_panicking_system_is_contained() {
        let mut engine = Engine::new();
        engine.register_system("panics", || -> Result<(), crate::error::SystemError> {
            panic!("deliberate panic from a system under test");
        });
        let handle = &mut engine as *mut Engine as *mut c_void;

        // The default panic hook would print the payload and clutter the test
        // output; the panic itself is the point, not its report.
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        // SAFETY: `handle` addresses a live engine for the duration of the call.
        let status = unsafe { api_process_frame(handle) };
        std::panic::set_hook(previous_hook);

        assert_eq!(
            status, -1,
            "a caught panic must surface as the failure value"
        );
    }
}
