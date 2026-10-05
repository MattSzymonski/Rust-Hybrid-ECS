//! The module entry-point contract: the table a loaded artifact receives.
//!
//! # Responsibilities
//!
//! - Defines [`EngineApi`], the `#[repr(C)]` struct every project and module
//!   DLL receives through its `pill_module_init` export.
//! - Constructs one for a live engine via [`EngineApi::new`].
//!
//! # Design
//!
//! **The module ABI is Rust-to-Rust by design.** The host owns the
//! [`Engine`](crate::Engine) and hands each loaded artifact a pointer to it;
//! the artifact casts `engine_handle` back to `&mut Engine` and calls the full
//! typed API. There is no language-neutral plugin ABI here, and none is
//! planned: a C or Zig module would need a function-pointer vocabulary for
//! every engine operation, and the engine's surface is generic Rust
//! (`register_component::<T>`, typed queries) that such a vocabulary cannot
//! express without a parallel implementation of the whole API.
//!
//! An earlier design advertised one: a seven-function C table of which two
//! entries were documented as unimplemented and the other five were never
//! called by anything but their own tests. It was removed rather than
//! completed, because what it cost was not the code but the reader's model -
//! `EngineApi` looked like a contract to understand and was not one.
//!
//! A `#[repr(C)]` struct rather than a bare pointer is still the right shape:
//! it is what crosses the DLL boundary, and it leaves room to add a field
//! without changing every entry point's signature. Adding or removing a field
//! changes a layout every module compiles against, so it is the same class of
//! coordination as an interop-contract version bump - every module and project
//! must be rebuilt in lockstep. In development that is automatic, because the
//! host builds them all from source at startup.

// Standard library
use std::ffi::c_void;

// Current crate
use crate::engine::Engine;

// =============================================================================
// EngineApi
// =============================================================================

/// The table a project or module DLL receives at its entry point.
///
/// # Safety
///
/// `engine_handle` is only valid while the host's [`Engine`](crate::Engine)
/// lives and while the artifact is loaded. The artifact must not store it
/// beyond the duration of the `pill_module_init` / `pill_module_update` call.
///
/// # Examples
///
/// ```ignore
/// #[no_mangle]
/// pub unsafe extern "C" fn pill_module_init(api: *const EngineApi) -> u32 {
///     let api = unsafe { &*api };
///     let engine: &mut Engine = unsafe { &mut *(api.engine_handle as *mut Engine) };
///     engine.register_component::<Position>();
///     engine.register_system("movement", movement_system);
///     0
/// }
/// ```
///
/// In practice no artifact writes that by hand: `#[pill_project]` and
/// `#[pill_module]` generate the entry point, including the `catch_unwind`
/// guard that keeps a panic from unwinding across the C ABI boundary.
#[repr(C)]
pub struct EngineApi {
    /// Opaque handle to the engine instance, cast back to `&mut Engine` by the
    /// receiving artifact.
    pub engine_handle: *mut c_void,
}

impl EngineApi {
    /// Build the table that addresses the given engine.
    ///
    /// The engine must outlive every artifact that receives the result, which
    /// is why the host boxes its engine before constructing this: the handle is
    /// a raw pointer into that allocation and moving the host would invalidate
    /// it.
    pub fn new(engine: &mut Engine) -> Self {
        Self {
            engine_handle: engine as *mut Engine as *mut c_void,
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The handle addresses the engine it was built from.
    ///
    /// This is the whole contract the generated entry points rely on: every
    /// `pill_module_init` casts this field back and expects the host's engine.
    #[test]
    fn the_handle_casts_back_to_the_engine_it_was_built_from() {
        let mut engine = Engine::new();
        let expected = &mut engine as *mut Engine;
        let api = EngineApi::new(&mut engine);

        assert_eq!(
            api.engine_handle as *mut Engine, expected,
            "the handle must address the engine it was built from"
        );

        // SAFETY: the handle was just built from `engine`, which is still alive
        // and not otherwise borrowed, so this is the same unique reference the
        // generated entry points reconstruct.
        let restored: &mut Engine = unsafe { &mut *(api.engine_handle as *mut Engine) };
        assert_eq!(
            restored.world().entity_count(),
            0,
            "a fresh world is reachable through the handle"
        );
    }

    /// The table's layout is what every module DLL compiles against, so a
    /// change to it has to be a deliberate one: this pins the shape that
    /// forces a lockstep rebuild.
    #[test]
    fn the_table_is_one_pointer_wide() {
        assert_eq!(
            std::mem::size_of::<EngineApi>(),
            std::mem::size_of::<*mut c_void>(),
            "EngineApi carries exactly the engine handle"
        );
        assert_eq!(
            std::mem::align_of::<EngineApi>(),
            std::mem::align_of::<*mut c_void>(),
        );
    }
}
