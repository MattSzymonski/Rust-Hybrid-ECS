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
//! invocation. Entity rows arrive in `ComponentChunk::entities`, a const slot,
//! and only for an archetype a validated term already served in the same
//! invocation - the entity path cannot be used to probe arbitrary archetypes.

// External crates
use pill_core::telemetry::telemetry_target;
use pill_engine::archetype::ArchetypeId;
use pill_engine::Entity;

// Current crate
use super::abi::ComponentChunk;
use super::components::{ComponentBinding, StableComponentId};
use super::context::{
    access_is_authorized, archetype_was_observed, record_observed_archetype, with_active_context,
    with_active_world,
};

// =============================================================================
// Free Functions
// =============================================================================

/// Record the archetype of a chunk a validated term just served.
///
/// The entity path answers only for archetypes recorded here, so the recording
/// happens exactly where a chunk is returned - a status other than success
/// served nothing and records nothing.
fn record_served_archetype(status: u8, output: *mut ComponentChunk) {
    if status != 1 {
        return;
    }
    // SAFETY: a status of `1` means the arm that produced it wrote a complete
    // chunk into the output buffer, which was checked non-null at entry.
    let chunk = unsafe { &*output };
    record_observed_archetype(ArchetypeId(
        ((chunk.archetype_high as u128) << 64) | chunk.archetype_low as u128,
    ));
}
/// Return a component chunk to managed code after validating scheduler access.
///
/// Status codes are interpreted by `Engine.TryGetChunk`: `0` ends iteration,
/// `1` returns a chunk, `2` is unknown component, `3` is out-of-scope access,
/// `4` is an undeclared access mode, and `5` reports a caller bug - the output
/// buffer is null, which the managed side maps to an `ArgumentException`.
pub(super) extern "C" fn ffi_get_component_chunk(
    key_low: u64,
    key_high: u64,
    mode: u8,
    chunk_index: u32,
    output: *mut ComponentChunk,
) -> u8 {
    // Step 1: Reject a null output buffer - the caller-bug status, not the
    // end-of-iteration code - and validate the declared access mode.
    if output.is_null() {
        return 5;
    }
    let stable_id = StableComponentId::from_halves(key_low, key_high);
    match access_is_authorized(stable_id, mode) {
        None => return 3,
        Some(false) => return 4,
        Some(true) => {}
    }

    // Step 2: Resolve the component binding and write the requested chunk
    // into the managed caller's output buffer.
    let status = with_active_context(|world, bindings| match bindings.get(&stable_id).copied() {
        Some(ComponentBinding::Native { get_chunk, .. }) => get_chunk(world, chunk_index, output),
        Some(ComponentBinding::Dynamic { component_id, .. }) => {
            // The stride comes from the live column, never from the binding's
            // copy of the layout: `data` points into that column, and only the
            // column describes what is stored there. A binding that drifted
            // must not be able to make managed row arithmetic walk off the
            // column; a missing column fails closed like an unknown component.
            let Some((live_size, _)) = world.component_layout(component_id) else {
                return 2;
            };
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
                    entities: std::ptr::null(),
                    len: len as u32,
                    element_size: live_size as u32,
                    ticks: ticks.as_mut_ptr(),
                    change_tick,
                });
            }
            1
        }
        Some(ComponentBinding::ModuleNative {
            component_id, size, ..
        }) => {
            let change_tick = world.change_tick().get();
            // The native twin of the dynamic path: serve the module's native
            // column as raw bytes so managed code and Rust share one storage.
            let Some((archetype, data, len, element_size, ticks)) =
                world.native_component_chunk_mut(component_id, chunk_index as usize)
            else {
                return 0;
            };
            // The binding was built from a layout the live column no longer
            // matches - the module reloaded with a new schema, or a mirror
            // arrived against a stale one. A wrong-size chunk would be read
            // as the managed mirror's fields, so the request fails closed
            // here; this replaces a `debug_assert_eq!` that aborted debug
            // hosts and vanished entirely in release.
            if element_size != size {
                pill_core::error!(
                    target: telemetry_target::ECS,
                    component_id = ?component_id,
                    binding_size = size,
                    column_size = element_size,
                    "module native binding size disagrees with the live column"
                );
                return 2;
            }
            let bits = archetype.0;
            // SAFETY: identical to the dynamic arm above - pointers stay owned
            // by the active world's archetype for the managed invocation.
            unsafe {
                output.write(ComponentChunk {
                    archetype_low: bits as u64,
                    archetype_high: (bits >> 64) as u64,
                    data: data.cast(),
                    entities: std::ptr::null(),
                    len: len as u32,
                    element_size: element_size as u32,
                    ticks: ticks.as_mut_ptr(),
                    change_tick,
                });
            }
            1
        }
        None => 2,
    })
    .unwrap_or(3);
    record_served_archetype(status, output);
    status
}

/// Return one component chunk of an archetype the managed enumerator already
/// identified through its driver chunk.
///
/// Managed query iterators resolve every non-driver term with this call once
/// per archetype, instead of scanning chunk indices until the archetypes
/// match. Status codes match [`ffi_get_component_chunk`]: `0` means the
/// archetype does not carry the component (an optional term is absent), `1`
/// returns a chunk, `2` is unknown component, `3` is out-of-scope access,
/// `4` is an undeclared access mode, and `5` reports a null output buffer. A
/// `mode` of `2` requests the archetype's entity column instead, which carries
/// no component access to validate.
pub(super) extern "C" fn ffi_get_archetype_chunk(
    archetype_low: u64,
    archetype_high: u64,
    key_low: u64,
    key_high: u64,
    mode: u8,
    output: *mut ComponentChunk,
) -> u8 {
    // Step 1: Reject a null output buffer before reconstructing the identity;
    // the caller-bug status, not end-of-iteration.
    if output.is_null() {
        return 5;
    }
    let archetype_id = ArchetypeId(((archetype_high as u128) << 64) | archetype_low as u128);

    // Step 2: Entity terms resolve against the archetype's entity column and
    // carry no component access, matching the dedicated entity callback. The
    // archetype must already have been served through a validated term in this
    // same invocation: without that precondition any id could be probed, which
    // is a component-set oracle no query declared.
    if mode == 2 {
        match archetype_was_observed(archetype_id) {
            None => return 3,
            Some(false) => return 4,
            Some(true) => {}
        }
        let status = with_active_world(|world| {
            let Some((archetype, entities)) = world.entity_chunk_in_archetype(archetype_id) else {
                return 0;
            };
            let bits = archetype.0;
            // SAFETY: `output` was checked above and the entity slice remains
            // borrowed only for the active managed system invocation. Entity
            // rows are exposed through the const `entities` slot, so nothing
            // managed can write through them.
            unsafe {
                output.write(ComponentChunk {
                    archetype_low: bits as u64,
                    archetype_high: (bits >> 64) as u64,
                    data: std::ptr::null_mut(),
                    entities: entities.as_ptr().cast(),
                    len: entities.len() as u32,
                    element_size: std::mem::size_of::<Entity>() as u32,
                    ticks: std::ptr::null_mut(),
                    change_tick: world.change_tick().get(),
                });
            }
            1
        })
        .unwrap_or(3);
        record_served_archetype(status, output);
        return status;
    }

    // Step 3: Component terms validate the declared access exactly like the
    // index-based lookup, so both entry points fail the same way.
    let stable_id = StableComponentId::from_halves(key_low, key_high);
    match access_is_authorized(stable_id, mode) {
        None => return 3,
        Some(false) => return 4,
        Some(true) => {}
    }

    // Step 4: Resolve the binding and write the column from that one
    // archetype; the remaining chunks of the world are never touched.
    let status = with_active_context(|world, bindings| match bindings.get(&stable_id).copied() {
        Some(ComponentBinding::Native {
            get_chunk_in_archetype,
            ..
        }) => get_chunk_in_archetype(world, archetype_id, output),
        Some(ComponentBinding::Dynamic { component_id, .. }) => {
            // As in the index-based callback: the stride is the live column's,
            // so a drifted binding cannot mislead managed row arithmetic.
            let Some((live_size, _)) = world.component_layout(component_id) else {
                return 2;
            };
            let change_tick = world.change_tick().get();
            let Some((archetype, data, len, ticks)) =
                world.dynamic_component_chunk_in_archetype(component_id, archetype_id)
            else {
                return 0;
            };
            let bits = archetype.0;
            // SAFETY: `output` is non-null and the pointers stay owned by the
            // active world's archetype for the managed invocation, exactly as
            // in the index-based callback above.
            unsafe {
                output.write(ComponentChunk {
                    archetype_low: bits as u64,
                    archetype_high: (bits >> 64) as u64,
                    data: data.cast(),
                    entities: std::ptr::null(),
                    len: len as u32,
                    element_size: live_size as u32,
                    ticks: ticks.as_mut_ptr(),
                    change_tick,
                });
            }
            1
        }
        Some(ComponentBinding::ModuleNative {
            component_id, size, ..
        }) => {
            let change_tick = world.change_tick().get();
            let Some((archetype, data, len, element_size, ticks)) =
                world.native_component_chunk_in_archetype(component_id, archetype_id)
            else {
                return 0;
            };
            // As in the index-based callback above: a binding that no longer
            // matches the live column fails closed instead of asserting.
            if element_size != size {
                pill_core::error!(
                    target: telemetry_target::ECS,
                    component_id = ?component_id,
                    binding_size = size,
                    column_size = element_size,
                    "module native binding size disagrees with the live column"
                );
                return 2;
            }
            let bits = archetype.0;
            // SAFETY: identical to the dynamic arm above.
            unsafe {
                output.write(ComponentChunk {
                    archetype_low: bits as u64,
                    archetype_high: (bits >> 64) as u64,
                    data: data.cast(),
                    entities: std::ptr::null(),
                    len: len as u32,
                    element_size: element_size as u32,
                    ticks: ticks.as_mut_ptr(),
                    change_tick,
                });
            }
            1
        }
        None => 2,
    })
    .unwrap_or(3);
    record_served_archetype(status, output);
    status
}

/// Return the `chunk_index`th archetype entity column.
///
/// The slice written to `output` contains `Entity` values for one archetype
/// and is only valid while the managed system that triggered the callback is
/// active; callers must not retain it beyond the invocation.
pub(super) extern "C" fn ffi_get_entity_chunk(chunk_index: u32, output: *mut ComponentChunk) -> u8 {
    // Step 1: Reject a null output buffer before touching the world; the
    // caller-bug status, not end-of-iteration.
    if output.is_null() {
        return 5;
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
        // borrowed only for the active managed system invocation. Entity rows
        // are exposed through the const `entities` slot, so nothing managed can
        // write through them.
        unsafe {
            output.write(ComponentChunk {
                archetype_low: bits as u64,
                archetype_high: (bits >> 64) as u64,
                data: std::ptr::null_mut(),
                entities: entities.as_ptr().cast(),
                len: entities.len() as u32,
                element_size: std::mem::size_of::<Entity>() as u32,
                ticks: std::ptr::null_mut(),
                change_tick: world.change_tick().get(),
            });
        }
        record_observed_archetype(archetype);
        1
    })
    .unwrap_or(3)
}

/// Write the current entity count while a managed system is active.
///
/// Status codes: `0` wrote the count, `3` no managed system is scheduled (a
/// caller cannot tell that from an empty world through the count alone, so it
/// gets a status the managed wrapper turns into an exception), and `5` the
/// caller passed no output buffer.
///
/// The count is `u32` by ABI design; worlds above ~4.29 billion entities are
/// unsupported (see the `ComponentChunk` layout-limits documentation).
pub(super) extern "C" fn ffi_entity_count(output: *mut u32) -> u8 {
    if output.is_null() {
        return 5;
    }
    let Some(count) = with_active_world(|world| world.entity_count() as u32) else {
        return 3;
    };
    // SAFETY: `output` was checked non-null above and points at a caller-owned
    // `u32` for the duration of this call.
    unsafe { output.write(count) };
    0
}
