//! C-compatible data structures and callback table shared with `csharp_runtime`.
//!
//! Field order, field widths, and calling conventions in this module are part
//! of the native/managed ABI and must remain synchronized with `EngineApi.cs`.
//!
//! # Responsibilities
//!
//! - Define the C-compatible function table copied by `csharp_runtime`.
//! - Describe component blobs, chunks, and scheduler access for managed code.
//!
//! # Design
//!
//! [`CsEngineApi`] is the single entry point the managed runtime copies at
//! startup; every slot is one `extern "C"` callback implemented by the query
//! and deferred-command adapters in this module's sibling modules.
//! [`NativeComponentBlob`] and [`ComponentChunk`] describe engine-owned
//! buffers whose validity is bound to the managed invocation that received
//! them, while [`NativeSystemAccess`] carries one scheduler permission per
//! queried component.

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
/// deferred-command adapters in this crate. `queue_create` accepts at most
/// `MAX_COMPONENTS_PER_CREATE` component blobs per call.
#[repr(C)]
pub(super) struct CsEngineApi {
    /// Number of live entities in the active world.
    entity_count: extern "C" fn() -> u32,
    /// Fill a [`ComponentChunk`] for one archetype/component pair.
    get_component_chunk: extern "C" fn(u64, u64, u8, u32, *mut ComponentChunk) -> u8,
    /// Fill a [`ComponentChunk`] describing the entity column.
    get_entity_chunk: extern "C" fn(u32, *mut ComponentChunk) -> u8,
    /// Reserve a generation-checked entity handle without inserting it.
    reserve_entity: extern "C" fn(*mut Entity) -> u8,
    /// Queue creation of an entity with up to `MAX_COMPONENTS_PER_CREATE` blobs.
    queue_create: extern "C" fn(*const Entity, *const NativeComponentBlob, u32) -> u8,
    /// Queue destruction of an entity.
    queue_destroy: extern "C" fn(*const Entity) -> u8,
    /// Queue addition of one component to an entity.
    queue_add_component: extern "C" fn(*const Entity, u64, u64, *const u8, u32) -> u8,
    /// Queue removal of one component from an entity.
    queue_remove_component: extern "C" fn(*const Entity, u64, u64) -> u8,
}

impl CsEngineApi {
    /// Assembles the stable function table from the query and deferred-command
    /// adapter callbacks.
    ///
    /// Every slot is wired to the matching `ffi_*` adapter so the managed
    /// runtime never observes Rust-internal representation details.
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
    /// Low half of the stable component key identifying the component type.
    pub(super) component_key: u64,
    /// High half of the stable component key identifying the component type.
    pub(super) component_key_high: u64,
    /// Pointer to the managed-pinned component bytes.
    pub(super) data: *const u8,
    /// Byte length of the pinned buffer pointed to by `data`.
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
///
/// # Layout limits
///
/// `len` is `u32` by ABI design; worlds larger than ~4.29 billion entities
/// per column are unsupported and must not be relied upon to wrap.
#[repr(C)]
pub(super) struct ComponentChunk {
    /// Low half of the archetype identifier the column belongs to.
    pub(super) archetype_low: u64,
    /// High half of the archetype identifier the column belongs to.
    pub(super) archetype_high: u64,
    /// Pointer to the first element of the contiguous column storage.
    pub(super) data: *mut std::ffi::c_void,
    /// Number of elements stored in the column.
    pub(super) len: u32,
    /// Byte size of a single column element.
    pub(super) element_size: u32,
    /// Per-element change ticks; null for read-only or entity columns.
    pub(super) ticks: *mut ComponentTicks,
    /// World tick of the last modification to this column.
    pub(super) change_tick: u32,
}

/// One reflected scheduler access, where `0` is read and `1` is write.
///
/// Managed systems declare one entry per queried component before the native
/// side derives scheduler metadata.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct NativeSystemAccess {
    /// Low half of the component key the access entry refers to.
    pub(super) component_key: u64,
    /// High half of the component key the access entry refers to.
    pub(super) component_key_high: u64,
    /// Access mode: `0` is read-only and `1` is read-write.
    pub(super) mode: u8,
}
