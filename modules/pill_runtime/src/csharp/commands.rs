//! Native callbacks that translate C# lifecycle requests into deferred ECS commands.
//!
//! # Responsibilities
//!
//! - Reserve generation-checked entity handles for managed callers.
//! - Decode managed component blobs into native or dynamic adders.
//! - Queue create, destroy, add, and remove operations against the active
//!   world, rejecting stale generations and undeclared scopes.
//!
//! # Design
//!
//! Every callback funnels through [`with_active_command_context`], which
//! resolves the active world once and hands the closure a queue, a binding
//! table, and the invocation-scope reservation set. Component blobs are
//! decoded through [`ComponentBinding`] into either a native [`ComponentAdder`]
//! or a type-erased [`DecodedCommandComponent`] before being handed to the
//! queue, so managed callers never touch concrete Rust types directly.

// Standard library
use std::collections::HashSet;

// External crates
use pill_engine::commands::ComponentAdder;
use pill_engine::{ComponentId, Entity};

// Current crate
use super::abi::NativeComponentBlob;
use super::components::{ComponentBinding, StableComponentId};
use super::context::{active_world_exists, with_active_command_context};

// =============================================================================
// Constants
// =============================================================================

/// Maximum number of component blobs one managed create may carry.
///
/// Part of the shared ABI contract: `queue_create` rejects larger counts
/// with an error status, and the managed side must respect the same bound.
pub(super) const MAX_COMPONENTS_PER_CREATE: u32 = 1024;

// =============================================================================
// Types
// =============================================================================

/// A decoded command component in its native or type-erased representation.
///
/// The native variant carries a fully typed [`ComponentAdder`] produced by the
/// binding's decode function; the dynamic variant carries a stable component
/// identity plus raw bytes for late binding by the queue.
enum DecodedCommandComponent {
    /// A concrete Rust component decoded through its native binding.
    Native(Box<dyn ComponentAdder>),
    /// A type-erased byte payload for a dynamically registered component.
    Dynamic(ComponentId, Vec<u8>),
}

// =============================================================================
// Free Functions
// =============================================================================

/// Return the command-specific scope error used by all lifecycle callbacks.
///
/// Reports 3 when no world is active on this thread, or 4 when a world exists
/// but the command scope cannot be entered.
fn command_scope_error() -> u8 {
    if active_world_exists() {
        4
    } else {
        3
    }
}

/// Reserve a generation-checked handle without inserting it into an archetype.
///
/// Reservations that are never consumed by a create are returned to the
/// entity allocator automatically when the invocation scope ends.
pub(super) extern "C" fn ffi_reserve_entity(output: *mut Entity) -> u8 {
    if output.is_null() {
        return 6;
    }
    with_active_command_context(|world, _queue, _bindings, reserved| {
        // Step 1: allocate a fresh generation-checked entity handle.
        let entity = world.reserve_entity();
        // Step 2: track the handle so an unconsumed reservation is returned
        // to the allocator when the invocation scope ends.
        reserved.insert(entity);
        // Step 3: write the handle back through the managed out pointer.
        // SAFETY: managed code supplied a checked, non-null out pointer whose
        // ABI layout is validated by the managed Entity mirror.
        unsafe { output.write(entity) };
        1
    })
    .unwrap_or_else(command_scope_error)
}

/// Validate and copy a component blob into the representation understood by
/// the native or type-erased branch of the Rust command queue.
///
/// # Errors
///
/// Returns an error string when the data pointer is null or when the blob
/// size does not match the size declared by the resolved binding.
fn decode_command_component(
    binding: ComponentBinding,
    data: *const u8,
    size: usize,
) -> Result<DecodedCommandComponent, String> {
    if data.is_null() {
        return Err("component data pointer is null".into());
    }
    match binding {
        ComponentBinding::Native {
            size: expected,
            decode,
            ..
        } => {
            if size != expected {
                return Err("native component blob has the wrong size".into());
            }
            decode(data, size).map(DecodedCommandComponent::Native)
        }
        ComponentBinding::Dynamic {
            component_id,
            size: expected,
            ..
        } => {
            if size != expected {
                return Err("dynamic component blob has the wrong size".into());
            }
            // SAFETY: the managed caller pins a buffer of exactly `size`
            // bytes for this callback; the bytes are copied before returning.
            let bytes = unsafe { std::slice::from_raw_parts(data, size) }.to_vec();
            Ok(DecodedCommandComponent::Dynamic(component_id, bytes))
        }
    }
}

/// Queue creation after validating every supplied component blob atomically.
///
/// Rejects more than [`MAX_COMPONENTS_PER_CREATE`] blobs as part of the
/// documented ABI contract.
pub(super) extern "C" fn ffi_queue_create(
    entity: *const Entity,
    blobs: *const NativeComponentBlob,
    count: u32,
) -> u8 {
    if entity.is_null() || (count != 0 && blobs.is_null()) || count > MAX_COMPONENTS_PER_CREATE {
        return 6;
    }
    with_active_command_context(|_world, queue, bindings, reserved| {
        // Step 1: recover the reserved entity handle, rejecting unknown ones.
        // SAFETY: the entity pointer is non-null (checked above) and managed
        // code pins the entity storage for this synchronous call.
        let entity = unsafe { *entity };
        if !reserved.contains(&entity) {
            return 5;
        }
        // Step 2: view the descriptor array as a slice. The descriptor
        // pointer is legitimately null when the managed caller supplied no
        // components, so the zero-count case never builds a raw slice.
        let blobs: &[NativeComponentBlob] = if count == 0 {
            &[]
        } else {
            // SAFETY: `blobs` is non-null whenever `count` is non-zero (checked
            // above) and `count` is capped at MAX_COMPONENTS_PER_CREATE, so the
            // slice stays within the array the managed caller pinned for this call.
            unsafe { std::slice::from_raw_parts(blobs, count as usize) }
        };
        // Step 3: decode every blob into a native or dynamic adder, rejecting
        // duplicate identities, undeclared bindings, and malformed payloads.
        let mut seen = HashSet::with_capacity(blobs.len());
        let mut native = Vec::new();
        let mut dynamic = Vec::new();
        for blob in blobs {
            let stable =
                StableComponentId::from_halves(blob.component_key, blob.component_key_high);
            if !seen.insert(stable) {
                return 6;
            }
            let Some(binding) = bindings.get(&stable).copied() else {
                return 2;
            };
            match decode_command_component(binding, blob.data, blob.size as usize) {
                Ok(DecodedCommandComponent::Native(component)) => native.push(component),
                Ok(DecodedCommandComponent::Dynamic(id, bytes)) => dynamic.push((id, bytes)),
                Err(_) => return 6,
            }
        }
        // Step 4: commit the validated creation and drop the reservation so
        // the handle can be recycled by the allocator.
        reserved.remove(&entity);
        queue.create_mixed_entity(entity, native, dynamic);
        1
    })
    .unwrap_or_else(command_scope_error)
}

/// Queue destruction only for a currently live generation.
pub(super) extern "C" fn ffi_queue_destroy(entity: *const Entity) -> u8 {
    if entity.is_null() {
        return 6;
    }
    with_active_command_context(|world, queue, _bindings, _reserved| {
        // Step 1: validate the entity handle against the live world.
        // SAFETY: the entity pointer is non-null (checked above) and managed
        // code pins the entity storage for this synchronous call.
        let entity = unsafe { *entity };
        if !world.is_entity_valid(entity) {
            return 5;
        }
        // Step 2: enqueue destruction for the validated handle.
        queue.destroy_entity(entity);
        1
    })
    .unwrap_or_else(command_scope_error)
}

/// Queue a component addition selected by stable managed identity.
pub(super) extern "C" fn ffi_queue_add_component(
    entity: *const Entity,
    key_low: u64,
    key_high: u64,
    data: *const u8,
    size: u32,
) -> u8 {
    if entity.is_null() {
        return 6;
    }
    with_active_command_context(|world, queue, bindings, _reserved| {
        // Step 1: validate the entity handle against the live world.
        // SAFETY: the entity pointer is non-null (checked above) and managed
        // code pins the entity storage for this synchronous call.
        let entity = unsafe { *entity };
        if !world.is_entity_valid(entity) {
            return 5;
        }
        // Step 2: resolve the stable component identity to a registered binding.
        let stable = StableComponentId::from_halves(key_low, key_high);
        let Some(binding) = bindings.get(&stable).copied() else {
            return 2;
        };
        // Step 3: decode the payload and enqueue the matching command.
        match decode_command_component(binding, data, size as usize) {
            Ok(DecodedCommandComponent::Native(component)) => {
                queue.add_component_adder_to_entity(entity, component)
            }
            Ok(DecodedCommandComponent::Dynamic(component_id, bytes)) => {
                queue.add_dynamic_component_to_entity(entity, component_id, bytes)
            }
            Err(_) => return 6,
        }
        1
    })
    .unwrap_or_else(command_scope_error)
}

/// Queue component removal without requiring a concrete Rust type.
pub(super) extern "C" fn ffi_queue_remove_component(
    entity: *const Entity,
    key_low: u64,
    key_high: u64,
) -> u8 {
    if entity.is_null() {
        return 6;
    }
    with_active_command_context(|world, queue, bindings, _reserved| {
        // Step 1: validate the entity handle against the live world.
        // SAFETY: the entity pointer is non-null (checked above) and managed
        // code pins the entity storage for this synchronous call.
        let entity = unsafe { *entity };
        if !world.is_entity_valid(entity) {
            return 5;
        }
        // Step 2: resolve the stable component identity to a registered binding.
        let stable = StableComponentId::from_halves(key_low, key_high);
        let Some(binding) = bindings.get(&stable).copied() else {
            return 2;
        };
        // Step 3: enqueue removal by the binding's native component identity.
        queue.remove_component_by_id(entity, binding.component_id());
        1
    })
    .unwrap_or_else(command_scope_error)
}
