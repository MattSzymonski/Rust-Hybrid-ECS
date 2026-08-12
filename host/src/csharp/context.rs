//! Thread-local access scope installed around one scheduled C# system.
//!
//! # Responsibilities
//!
//! - Publish the active world, queue, and bindings for managed callbacks.
//! - Reject nested or out-of-scope managed invocation.
//! - Clear the complete thread-local scope when the invocation ends.
//!
//! # Design
//!
//! All invocation state is bundled into one [`ActiveScopeData`] value held in
//! a single thread-local [`Cell`]. Installation and teardown are each one
//! assignment, which makes the scope panic-atomic by construction: a panic
//! can never leave a half-installed set of raw pointers behind.

// Standard library
use std::cell::{Cell, RefCell};
use std::collections::HashSet;

// External crates
use pill_engine::commands::CommandQueue;
use pill_engine::{Entity, World};

// Current crate
use super::abi::NativeSystemAccess;
use super::components::{ComponentBindings, StableComponentId};

thread_local! {
    /// Complete managed invocation scope for the current thread.
    ///
    /// `None` means no C# system is executing on this thread.
    static ACTIVE_SCOPE: Cell<Option<ActiveScopeData>> = const { Cell::new(None) };
    /// Handles reserved during this invocation and not yet consumed by create.
    static ACTIVE_RESERVED: RefCell<HashSet<Entity>> = RefCell::new(HashSet::new());
}

// =============================================================================
// Types + Impls
// =============================================================================

/// One complete managed invocation scope.
///
/// Bundling every pointer into a single value keeps installation and teardown
/// to one [`Cell::set`] call each, so no panic can ever strand a partially
/// installed scope on this thread.
#[derive(Clone, Copy)]
struct ActiveScopeData {
    world: *mut World,
    queue: *mut CommandQueue,
    access: (*const NativeSystemAccess, usize),
    bindings: *const ComponentBindings,
    uses_commands: bool,
}

/// Clears the thread-local native scope even if managed execution unwinds.
pub(super) struct ActiveSystemGuard;

impl ActiveSystemGuard {
    /// Install a query-only invocation scope used by native bridge tests.
    ///
    /// Production systems use [`Self::set_with_commands`] because the scheduler
    /// always supplies a queue even when the managed signature omits Commands.
    #[cfg(test)]
    pub(super) fn set(
        world: &mut World,
        access: &[NativeSystemAccess],
        bindings: &ComponentBindings,
    ) -> Self {
        Self::set_inner(world, std::ptr::null_mut(), access, bindings, false)
    }

    /// Publish one scheduled system's world, command queue, bindings, and
    /// reflected access declaration for synchronous managed callbacks.
    pub(super) fn set_with_commands(
        world: &mut World,
        queue: &mut CommandQueue,
        access: &[NativeSystemAccess],
        bindings: &ComponentBindings,
        uses_commands: bool,
    ) -> Self {
        Self::set_inner(world, queue, access, bindings, uses_commands)
    }

    /// Install the complete scope in one assignment after rejecting nested
    /// invocation, which would overwrite active raw pointers.
    ///
    /// The nested-invocation check is the only fallible step and it runs
    /// before any mutation, so a panic there leaves no stale state behind.
    fn set_inner(
        world: &mut World,
        queue: *mut CommandQueue,
        access: &[NativeSystemAccess],
        bindings: &ComponentBindings,
        uses_commands: bool,
    ) -> Self {
        // Step 1: Reject nested invocation before touching any thread-local.
        ACTIVE_SCOPE.with(|slot| {
            assert!(slot.get().is_none(), "nested managed ECS system invocation");
        });

        // Step 2: Clear reservations from the previous invocation without
        // panicking. A reservation borrow can only exist while a scope is
        // active, which Step 1 has already rejected, so this is belt-and-braces.
        ACTIVE_RESERVED.with(|slot| {
            if let Ok(mut reserved) = slot.try_borrow_mut() {
                reserved.clear();
            }
        });

        // Step 3: Commit the whole scope in one assignment. Cell::set cannot
        // panic, so from this point on the guard's Drop owns the teardown.
        ACTIVE_SCOPE.with(|slot| {
            slot.set(Some(ActiveScopeData {
                world: world as *mut World,
                queue,
                access: (access.as_ptr(), access.len()),
                bindings: bindings as *const ComponentBindings,
                uses_commands,
            }));
        });
        Self
    }
}

impl Drop for ActiveSystemGuard {
    /// Clear the complete scope before the scheduler's native borrows expire.
    fn drop(&mut self) {
        // Clear per-invocation reservations first so no handle survives the
        // scope, then remove the scope in one assignment.
        ACTIVE_RESERVED.with(|slot| {
            if let Ok(mut reserved) = slot.try_borrow_mut() {
                reserved.clear();
            }
        });
        ACTIVE_SCOPE.with(|slot| slot.set(None));
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Run a callback with the active world, deferred queue, component bindings,
/// and reservation set only when the system declared a Commands parameter.
pub(super) fn with_active_command_context<R>(
    f: impl FnOnce(&mut World, &mut CommandQueue, &ComponentBindings, &mut HashSet<Entity>) -> R,
) -> Option<R> {
    ACTIVE_SCOPE.with(|scope_slot| {
        let scope = scope_slot.get()?;
        if !scope.uses_commands {
            return None;
        }
        let world = scope.world;
        let queue = scope.queue;
        let bindings = scope.bindings;
        if world.is_null() || queue.is_null() || bindings.is_null() {
            return None;
        }
        ACTIVE_RESERVED.with(|reserved| {
            // SAFETY: ActiveSystemGuard installs the scope for one scheduled
            // invocation and clears it before its native borrows expire.
            Some(unsafe {
                f(
                    &mut *world,
                    &mut *queue,
                    &*bindings,
                    &mut reserved.borrow_mut(),
                )
            })
        })
    })
}

/// Run a callback with the world belonging to the active managed invocation,
/// returning `None` when called outside scheduler-controlled execution.
pub(super) fn with_active_world<R>(f: impl FnOnce(&mut World) -> R) -> Option<R> {
    ACTIVE_SCOPE.with(|slot| {
        let pointer = slot.get().map(|scope| scope.world)?;
        // SAFETY: ActiveSystemGuard installs this pointer immediately before
        // managed invocation and clears it before the borrowed world expires.
        (!pointer.is_null()).then(|| unsafe { f(&mut *pointer) })
    })
}

/// Report whether a managed system scope exists without dereferencing its world.
pub(super) fn active_world_exists() -> bool {
    ACTIVE_SCOPE.with(|slot| slot.get().is_some_and(|scope| !scope.world.is_null()))
}

/// Run a callback with both the active world and its stable component binding
/// table, which must always be installed and cleared as one guard scope.
pub(super) fn with_active_context<R>(
    f: impl FnOnce(&mut World, &ComponentBindings) -> R,
) -> Option<R> {
    ACTIVE_SCOPE.with(|slot| {
        let scope = slot.get()?;
        if scope.world.is_null() || scope.bindings.is_null() {
            return None;
        }
        // SAFETY: ActiveSystemGuard installs and clears the complete scope
        // for exactly the managed invocation.
        Some(unsafe { f(&mut *scope.world, &*scope.bindings) })
    })
}

/// Check whether the active system declared the requested component mode.
///
/// A write declaration also permits reads; a read declaration never permits
/// writes. `None` means no managed system is currently active on this thread.
pub(super) fn access_is_authorized(key: StableComponentId, requested_mode: u8) -> Option<bool> {
    ACTIVE_SCOPE.with(|slot| {
        let (pointer, len) = slot.get()?.access;
        if pointer.is_null() {
            return None;
        }
        // SAFETY: ActiveSystemGuard stores a slice owned by the registered
        // system closure and clears the scope before that invocation returns.
        let accesses = unsafe { std::slice::from_raw_parts(pointer, len) };
        Some(accesses.iter().any(|access| {
            StableComponentId::from_halves(access.component_key, access.component_key_high) == key
                && (requested_mode == 0 || access.mode == requested_mode)
        }))
    })
}
