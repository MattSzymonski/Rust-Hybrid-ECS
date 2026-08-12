//! C-compatible data structures and callback table shared with `csharp_runtime`.
//!
//! Field order, field widths, and calling conventions in this module are part
//! of the native/managed ABI and must remain synchronized with `EngineApi.cs`.
//!
//! # Responsibilities
//!
//! - Define the C-compatible function table copied by `csharp_runtime`.
//! - Describe component blobs, chunks, and scheduler access for managed code.

// External crates
use pill_engine::{ComponentTicks, Entity};

// Current crate
use super::commands::{
    ffi_queue_add_component, ffi_queue_create, ffi_queue_destroy, ffi_queue_remove_component,
    ffi_reserve_entity,
};
use super::queries::{ffi_entity_count, ffi_get_component_chunk, ffi_get_entity_chunk};

// =============================================================================
// Types
// =============================================================================

/// Function table copied by `csharp_runtime` during managed initialization.
///
/// Every slot maps to one `extern "C"` callback implemented by the query and
/// deferred-command adapters in this crate.
#[repr(C)]
pub(super) struct CsEngineApi {
    entity_count: extern "C" fn() -> u32,
    get_component_chunk: extern "C" fn(u64, u64, u8, u32, *mut ComponentChunk) -> u8,
    get_entity_chunk: extern "C" fn(u32, *mut ComponentChunk) -> u8,
    reserve_entity: extern "C" fn(*mut Entity) -> u8,
    queue_create: extern "C" fn(*const Entity, *const NativeComponentBlob, u32) -> u8,
    queue_destroy: extern "C" fn(*const Entity) -> u8,
    queue_add_component: extern "C" fn(*const Entity, u64, u64, *const u8, u32) -> u8,
    queue_remove_component: extern "C" fn(*const Entity, u64, u64) -> u8,
}

impl CsEngineApi {
    /// Assemble the stable table from callbacks implemented by the query and
    /// deferred-command adapters.
    pub(super) fn new() -> Self {
        Self {
            entity_count: ffi_entity_count,
            get_component_chunk: ffi_get_component_chunk,
            get_entity_chunk: ffi_get_entity_chunk,
            reserve_entity: ffi_reserve_entity,
            queue_create: ffi_queue_create,
            queue_destroy: ffi_queue_destroy,
            queue_add_component: ffi_queue_add_component,
            queue_remove_component: ffi_queue_remove_component,
        }
    }
}

/// One pinned managed component value supplied to a deferred command.
///
/// Managed code pins the data buffer for the complete duration of the call
/// that consumes the blob.
#[repr(C)]
pub(super) struct NativeComponentBlob {
    pub(super) component_key: u64,
    pub(super) component_key_high: u64,
    pub(super) data: *const u8,
    pub(super) size: u32,
}

/// Description of one contiguous ECS column returned to managed code.
///
/// # Managed-side obligations
///
/// - `data` and `ticks` point into engine-owned storage and remain valid only
///   for the duration of the scheduled managed invocation that received them;
///   they must never be cached or dereferenced in a later invocation.
/// - Managed code may write through `data` only for components the system
///   declared with write access, and must stay within `len * element_size`.
/// - `ticks` may be null for read-only columns; entity columns always carry
///   null ticks and their `data` must never be written through.
#[repr(C)]
pub(super) struct ComponentChunk {
    pub(super) archetype_low: u64,
    pub(super) archetype_high: u64,
    pub(super) data: *mut std::ffi::c_void,
    pub(super) len: u32,
    pub(super) element_size: u32,
    pub(super) ticks: *mut ComponentTicks,
    pub(super) change_tick: u32,
}

/// One reflected scheduler access, where `0` is read and `1` is write.
///
/// Managed systems declare one entry per queried component before the native
/// side derives scheduler metadata.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct NativeSystemAccess {
    pub(super) component_key: u64,
    pub(super) component_key_high: u64,
    pub(super) mode: u8,
}
