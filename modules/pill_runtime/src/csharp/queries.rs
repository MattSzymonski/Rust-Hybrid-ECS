//! Native callbacks used by C# query enumerators.
//!
//! # Responsibilities
//!
//! - Return validated component chunks to managed query iterators.
//! - Return entity columns and live entity counts during scheduled systems.
//! - Enforce the active system's declared read/write access.
//!
//! # Design
//!
//! Each `ffi_*` function is an `extern "C"` entry point invoked across the
//! host/managed boundary by C# query iterators and scheduled systems. Access
//! validation is delegated to [`access_is_authorized`], and world/binding
//! lookups run through [`with_active_context`] and [`with_active_world`], so
//! every callback fails closed with status code `3` when no managed system is
//! active. Pointers returned in [`ComponentChunk`] remain owned by the active
//! world's archetype and are only valid for the duration of the managed
//! invocation.

// External crates
use pill_engine::Entity;

// Current crate
use super::abi::ComponentChunk;
use super::components::{ComponentBinding, StableComponentId};
use super::context::{access_is_authorized, with_active_context, with_active_world};

// =============================================================================
// Free Functions
// =============================================================================

/// Return a component chunk to managed code after validating scheduler access.
///
/// Status codes are interpreted by `Engine.TryGetChunk`: `0` ends iteration,
/// `1` returns a chunk, `2` is unknown component, `3` is out-of-scope access,
/// and `4` is an undeclared access mode.
pub(super) extern "C" fn ffi_get_component_chunk(
    key_low: u64,
    key_high: u64,
    mode: u8,
    chunk_index: u32,
    output: *mut ComponentChunk,
) -> u8 {
    // Step 1: Reject null output buffers and validate the declared access mode.
    if output.is_null() {
        return 0;
    }
    let stable_id = StableComponentId::from_halves(key_low, key_high);
    match access_is_authorized(stable_id, mode) {
        None => return 3,
        Some(false) => return 4,
        Some(true) => {}
    }

    // Step 2: Resolve the component binding and write the requested chunk
    // into the managed caller's output buffer.
    with_active_context(|world, bindings| match bindings.get(&stable_id).copied() {
        Some(ComponentBinding::Native { get_chunk, .. }) => get_chunk(world, chunk_index, output),
        Some(ComponentBinding::Dynamic {
            component_id, size, ..
        }) => {
            let change_tick = world.change_tick().get();
            let Some((archetype, data, len, ticks)) =
                world.dynamic_component_chunk_mut(component_id, chunk_index as usize)
            else {
                return 0;
            };
            // The archetype's 128-bit pattern is split into two `u64` halves
            // to match the ABI layout of `ComponentChunk`.
            let bits = archetype.0;
            // SAFETY: output is non-null and all pointers remain owned by the
            // active world's archetype for the managed invocation. The managed
            // side must not retain the pointers beyond that invocation and
            // must respect the declared access mode. The u32 length ceiling
            // is documented on `ComponentChunk`.
            unsafe {
                output.write(ComponentChunk {
                    archetype_low: bits as u64,
                    archetype_high: (bits >> 64) as u64,
                    data: data.cast(),
                    len: len as u32,
                    element_size: size as u32,
                    ticks: ticks.as_mut_ptr(),
                    change_tick,
                });
            }
            1
        }
        None => 2,
    })
    .unwrap_or(3)
}

/// Return the `chunk_index`th archetype entity column.
///
/// The slice written to `output` contains `Entity` values for one archetype
/// and is only valid while the managed system that triggered the callback is
/// active; callers must not retain it beyond the invocation.
pub(super) extern "C" fn ffi_get_entity_chunk(chunk_index: u32, output: *mut ComponentChunk) -> u8 {
    // Step 1: Reject a null output buffer before touching the world.
    if output.is_null() {
        return 0;
    }
    // Step 2: Resolve the entity column and write it into the output buffer.
    with_active_world(|world| {
        let Some((archetype, entities)) = world.entity_chunk(chunk_index as usize) else {
            return 0;
        };
        // The archetype's 128-bit pattern is split into two `u64` halves to
        // match the ABI layout of `ComponentChunk`.
        let bits = archetype.0;
        // SAFETY: `output` was checked above and the entity slice remains
        // borrowed only for the active managed system invocation. The managed
        // side must treat the returned pointers as read-only and must not
        // retain them beyond the invocation.
        unsafe {
            output.write(ComponentChunk {
                archetype_low: bits as u64,
                archetype_high: (bits >> 64) as u64,
                data: entities.as_ptr().cast_mut().cast(),
                len: entities.len() as u32,
                element_size: std::mem::size_of::<Entity>() as u32,
                ticks: std::ptr::null_mut(),
                change_tick: world.change_tick().get(),
            });
        }
        1
    })
    .unwrap_or(3)
}

/// Return the current entity count while a managed system is active.
///
/// The count is `u32` by ABI design; worlds above ~4.29 billion entities are
/// unsupported (see the `ComponentChunk` layout-limits documentation).
pub(super) extern "C" fn ffi_entity_count() -> u32 {
    with_active_world(|world| world.entity_count() as u32).unwrap_or(0)
}
