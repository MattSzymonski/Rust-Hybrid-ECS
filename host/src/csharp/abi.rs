//! C-compatible data structures and callback table shared with `csharp_runtime`.
//!
//! Field order, field widths, and calling conventions in this module are part
//! of the native/managed ABI and must remain synchronized with `EngineApi.cs`.

use pill_engine::{ComponentTicks, Entity};

use super::commands::{
    ffi_queue_add_component, ffi_queue_create, ffi_queue_destroy, ffi_queue_remove_component,
    ffi_reserve_entity,
};
use super::queries::{ffi_entity_count, ffi_get_component_chunk, ffi_get_entity_chunk};

/// Function table copied by `csharp_runtime` during managed initialization.
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
#[repr(C)]
pub(super) struct NativeComponentBlob {
    pub(super) component_key: u64,
    pub(super) component_key_high: u64,
    pub(super) data: *const u8,
    pub(super) size: u32,
}

/// Description of one contiguous ECS column returned to managed code.
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
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct NativeSystemAccess {
    pub(super) component_key: u64,
    pub(super) component_key_high: u64,
    pub(super) mode: u8,
}
