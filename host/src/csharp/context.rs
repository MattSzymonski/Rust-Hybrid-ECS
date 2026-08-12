//! Thread-local access scope installed around one scheduled C# system.
//!
//! # Responsibilities
//!
//! - Publish the active world, queue, and bindings for managed callbacks.
//! - Reject nested or out-of-scope managed invocation.
//! - Clear every thread-local slot when the invocation ends.

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
    /// World available only while the current thread executes a C# system.
    static ACTIVE_WORLD: Cell<*mut World> = const { Cell::new(std::ptr::null_mut()) };
    /// Access declaration belonging to that active C# system.
    static ACTIVE_ACCESS: Cell<(*const NativeSystemAccess, usize)> =
        const { Cell::new((std::ptr::null(), 0)) };
    /// Stable component bindings belonging to the active C# runtime.
    static ACTIVE_BINDINGS: Cell<*const ComponentBindings> = const { Cell::new(std::ptr::null()) };
    /// Deferred queue available only to systems declaring `Commands`.
    static ACTIVE_QUEUE: Cell<*mut CommandQueue> = const { Cell::new(std::ptr::null_mut()) };
    /// Handles reserved during this invocation and not yet consumed by create.
    static ACTIVE_RESERVED: RefCell<HashSet<Entity>> = RefCell::new(HashSet::new());
    /// Whether reflected system metadata declared the managed Commands parameter.
    static ACTIVE_USES_COMMANDS: Cell<bool> = const { Cell::new(false) };
}

// =============================================================================
// Types + Impls
// =============================================================================

/// Clears thread-local native access even if managed execution unwinds.
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

    /// Populate every thread-local slot as one atomic logical scope and reject
    /// nested managed invocation, which would overwrite active raw pointers.
    fn set_inner(
        world: &mut World,
        queue: *mut CommandQueue,
        access: &[NativeSystemAccess],
        bindings: &ComponentBindings,
        uses_commands: bool,
    ) -> Self {
        ACTIVE_WORLD.with(|slot| {
            assert!(slot.get().is_null(), "nested managed ECS system invocation");
            slot.set(world as *mut World);
        });
        ACTIVE_ACCESS.with(|slot| slot.set((access.as_ptr(), access.len())));
        ACTIVE_BINDINGS.with(|slot| slot.set(bindings));
        ACTIVE_QUEUE.with(|slot| slot.set(queue));
        ACTIVE_USES_COMMANDS.with(|slot| slot.set(uses_commands));
        ACTIVE_RESERVED.with(|slot| slot.borrow_mut().clear());
        Self
    }
}

impl Drop for ActiveSystemGuard {
    /// Clear every published pointer and per-invocation reservation before the
    /// scheduler's native borrows are allowed to expire.
    fn drop(&mut self) {
        ACTIVE_ACCESS.with(|slot| slot.set((std::ptr::null(), 0)));
        ACTIVE_BINDINGS.with(|slot| slot.set(std::ptr::null()));
        ACTIVE_QUEUE.with(|slot| slot.set(std::ptr::null_mut()));
        ACTIVE_USES_COMMANDS.with(|slot| slot.set(false));
        ACTIVE_RESERVED.with(|slot| slot.borrow_mut().clear());
        ACTIVE_WORLD.with(|slot| slot.set(std::ptr::null_mut()));
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
    if !ACTIVE_USES_COMMANDS.with(Cell::get) {
        return None;
    }
    ACTIVE_WORLD.with(|world_slot| {
        ACTIVE_QUEUE.with(|queue_slot| {
            ACTIVE_BINDINGS.with(|bindings_slot| {
                let world = world_slot.get();
                let queue = queue_slot.get();
                let bindings = bindings_slot.get();
                if world.is_null() || queue.is_null() || bindings.is_null() {
                    return None;
                }
                ACTIVE_RESERVED.with(|reserved| {
                    // SAFETY: all pointers are installed for one scheduled
                    // invocation and cleared before its native borrows expire.
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
        })
    })
}

/// Run a callback with the world belonging to the active managed invocation,
/// returning `None` when called outside scheduler-controlled execution.
pub(super) fn with_active_world<R>(f: impl FnOnce(&mut World) -> R) -> Option<R> {
    ACTIVE_WORLD.with(|slot| {
        let pointer = slot.get();
        // SAFETY: ActiveSystemGuard installs this pointer immediately before
        // managed invocation and clears it before the borrowed world expires.
        (!pointer.is_null()).then(|| unsafe { f(&mut *pointer) })
    })
}

/// Report whether a managed system scope exists without dereferencing its world.
pub(super) fn active_world_exists() -> bool {
    ACTIVE_WORLD.with(|slot| !slot.get().is_null())
}

/// Run a callback with both the active world and its stable component binding
/// table, which must always be installed and cleared as one guard scope.
pub(super) fn with_active_context<R>(
    f: impl FnOnce(&mut World, &ComponentBindings) -> R,
) -> Option<R> {
    ACTIVE_WORLD.with(|world_slot| {
        ACTIVE_BINDINGS.with(|bindings_slot| {
            let world = world_slot.get();
            let bindings = bindings_slot.get();
            if world.is_null() || bindings.is_null() {
                None
            } else {
                // SAFETY: ActiveSystemGuard installs and clears both pointers
                // for exactly the managed invocation.
                Some(unsafe { f(&mut *world, &*bindings) })
            }
        })
    })
}

/// Check whether the active system declared the requested component mode.
///
/// A write declaration also permits reads; a read declaration never permits
/// writes. `None` means no managed system is currently active on this thread.
pub(super) fn access_is_authorized(key: StableComponentId, requested_mode: u8) -> Option<bool> {
    ACTIVE_ACCESS.with(|slot| {
        let (pointer, len) = slot.get();
        if pointer.is_null() {
            return None;
        }
        // SAFETY: ActiveSystemGuard stores a slice owned by the registered
        // system closure and clears the slot before that invocation returns.
        let accesses = unsafe { std::slice::from_raw_parts(pointer, len) };
        Some(accesses.iter().any(|access| {
            StableComponentId::from_halves(access.component_key, access.component_key_high) == key
                && (requested_mode == 0 || access.mode == requested_mode)
        }))
    })
}
