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
use super::ResolvedMirrorMethod;

// =============================================================================
// Types
// =============================================================================

/// Host-side table of mirrored Rust methods handed to the managed runtime.
///
/// Set once per C# runtime startup by [`CsEngineApi::new`]; the two FFI
/// callbacks below read it. It is replaced on every (re)start, and the strings
/// are kept alive alongside the table for its whole lifetime. `Send`-safe by
/// construction (no raw pointers are stored; rows reference owned strings by
/// index), so the table can live in a `static`.
static MIRROR_METHOD_TABLE: std::sync::Mutex<Option<MirrorMethodTable>> =
    std::sync::Mutex::new(None);

/// One send-safe row of the mirror-method table.
struct MirrorMethodRow {
    /// Index into [`MirrorMethodTable::names`] of the fully-qualified Rust
    /// type name.
    type_name_index: usize,
    /// Index into [`MirrorMethodTable::names`] of the Rust method name.
    method_index: usize,
    /// Address of the exported `#[no_mangle]` C-ABI trampoline.
    address: usize,
}

/// Owned backing store for the mirror-method table exposed to managed code.
struct MirrorMethodTable {
    /// NUL-terminated names, kept alive so the exposed pointers stay valid.
    names: Vec<std::ffi::CString>,
    /// Send-safe rows; the copy callback materializes the C-ABI entries.
    rows: Vec<MirrorMethodRow>,
}

/// One mirrored Rust method the managed runtime can call.
///
/// Field order and widths are part of the native/managed ABI and must stay
/// synchronized with `EngineApi.cs`.
#[repr(C)]
pub(super) struct MirrorMethodEntry {
    /// Fully-qualified Rust type name (`pill_spline::OmoMO`), NUL-terminated.
    pub(super) type_name: *const std::ffi::c_char,
    /// Rust method name (`get_sum`), NUL-terminated.
    pub(super) method: *const std::ffi::c_char,
    /// Address of the exported `#[no_mangle]` C-ABI trampoline.
    pub(super) address: usize,
}

/// Report how many mirrored Rust methods are registered.
extern "C" fn ffi_mirror_method_count() -> u32 {
    MIRROR_METHOD_TABLE
        .lock()
        .map(|table| table.as_ref().map_or(0, |table| table.rows.len() as u32))
        .unwrap_or(0)
}

/// Copy up to `max` mirror-method rows into `out`; returns the count written.
///
/// # Safety
///
/// `out` must point at `max` writable [`MirrorMethodEntry`] slots owned by the
/// managed runtime for the duration of the call.
extern "C" fn ffi_copy_mirror_methods(out: *mut MirrorMethodEntry, max: u32) -> u32 {
    let Ok(table) = MIRROR_METHOD_TABLE.lock() else {
        return 0;
    };
    let Some(table) = table.as_ref() else {
        return 0;
    };
    let count = (table.rows.len() as u32).min(max);
    for (index, row) in table.rows.iter().take(count as usize).enumerate() {
        let entry = MirrorMethodEntry {
            type_name: table.names[row.type_name_index].as_ptr(),
            method: table.names[row.method_index].as_ptr(),
            address: row.address,
        };
        // SAFETY: `index < count <= max`, so `out.add(index)` stays inside the
        // buffer the caller promised, and `MirrorMethodEntry` is `Copy`.
        unsafe {
            out.add(index).write(entry);
        }
    }
    count
}

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
    /// Report how many mirrored Rust methods are registered.
    mirror_method_count: extern "C" fn() -> u32,
    /// Copy the mirrored-method rows into a managed-owned buffer.
    copy_mirror_methods: extern "C" fn(*mut MirrorMethodEntry, u32) -> u32,
}

impl CsEngineApi {
    /// Assembles the stable function table from the query and deferred-command
    /// adapter callbacks.
    ///
    /// Every slot is wired to the matching `ffi_*` adapter so the managed
    /// runtime never observes Rust-internal representation details. The
    /// resolved `#[pill_mirror_method]` trampolines are published through the
    /// two mirror-method slots.
    pub(super) fn new(mirror_methods: &[ResolvedMirrorMethod]) -> Self {
        publish_mirror_methods(mirror_methods);

        Self {
            entity_count: ffi_entity_count,
            get_component_chunk: ffi_get_component_chunk,
            get_entity_chunk: ffi_get_entity_chunk,
            reserve_entity: ffi_reserve_entity,
            queue_create: ffi_queue_create,
            queue_destroy: ffi_queue_destroy,
            queue_add_component: ffi_queue_add_component,
            queue_remove_component: ffi_queue_remove_component,
            mirror_method_count: ffi_mirror_method_count,
            copy_mirror_methods: ffi_copy_mirror_methods,
        }
    }
}

/// Rebuild the mirror-method table the managed runtime copies at startup.
///
/// Called both when the C# backend starts (from [`CsEngineApi::new`]) and when
/// an optional module reloads, so the C# side always resolves trampoline
/// addresses from the module generations currently loaded — a hot reload gives
/// the module a new image (and therefore new addresses), and a reloaded module
/// may have added or removed mirrored methods.
pub(crate) fn publish_mirror_methods(mirror_methods: &[ResolvedMirrorMethod]) {
    // Two NUL-terminated names per method, stored so the exposed pointers stay
    // valid for as long as the table lives. Rows reference the names by index
    // so the stored table stays `Send`.
    let mut names: Vec<std::ffi::CString> = Vec::new();
    let mut rows: Vec<MirrorMethodRow> = Vec::new();
    for method in mirror_methods {
        let Some(type_name) = std::ffi::CString::new(method.type_name.as_str()).ok() else {
            continue;
        };
        let Some(method_name) = std::ffi::CString::new(method.method_name.as_str()).ok() else {
            continue;
        };
        rows.push(MirrorMethodRow {
            type_name_index: names.len(),
            method_index: names.len() + 1,
            address: method.address,
        });
        names.push(type_name);
        names.push(method_name);
    }
    *MIRROR_METHOD_TABLE.lock().unwrap() = Some(MirrorMethodTable { names, rows });
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
